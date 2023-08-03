use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use chrono::TimeZone;
use eyre::ContextCompat;
use futures::{StreamExt, TryStreamExt};
use fuzzysearch_common::{Artist, Rating, Site, Submission};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::{process_file, LoadableSite, SubmissionResult};

pub struct Twitter {
    pool: PgPool,
    client: reqwest::Client,
    download_path: Option<PathBuf>,

    token: RwLock<Option<String>>,
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/109.0.0.0 Safari/537.36";
const SEC_CH_UA: &str = r#".Not/A)Brand";v="99", "Google Chrome";v="109", "Chromium";v="109"#;
const GUEST_BEARER_TOKEN: &str = "Bearer AAAAAAAAAAAAAAAAAAAAAPYXBAAAAAAACLXUNDekMxqa8h%2F40K4moUkGsoc%3DTYfbDKbT3jJPCEVnMYqilB28NHfOPqkca3qaAxGfsyKCs0wRbw";

impl Twitter {
    pub fn new(client: reqwest::Client, download_path: Option<PathBuf>, pool: PgPool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            client,
            download_path,
            token: Default::default(),
        })
    }

    fn headers() -> HeaderMap {
        [
            ("user-agent", USER_AGENT),
            ("sec-ch-ua", SEC_CH_UA),
            ("sec-ch-ua-mobile", "?0"),
            ("sec-ch-ua-platform", "Windows"),
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-dest", "empty"),
            ("x-twitter-active-user", "yes"),
            ("x-twitter-client-language", "en"),
            ("accept-encoding", "gzip, deflate, br"),
            ("accept-language", "en"),
            ("accept", "*/*"),
            ("content-type", "application/x-www-form-urlencoded"),
            ("dnt", "1"),
            ("origin", "https://twitter.com"),
            ("referer", "https://twitter.com/"),
        ]
        .into_iter()
        .map(|(name, value)| {
            (
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            )
        })
        .collect()
    }

    #[tracing::instrument(skip(self))]
    async fn refresh_token(&self, force: bool) -> eyre::Result<String> {
        {
            match self.token.read().await.clone() {
                Some(token) if !force => {
                    tracing::debug!("already had token, reusing");
                    return Ok(token);
                }
                _ => (),
            }
        }

        let mut headers = Self::headers();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static(GUEST_BEARER_TOKEN),
        );

        let mut attempt = 0;
        while attempt < 3 {
            tracing::debug!(attempt, "attempting to activate guest token");

            let resp: serde_json::Value = self
                .client
                .post("https://api.twitter.com/1.1/guest/activate.json")
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            if let Some(token) = resp.get("guest_token").and_then(|token| token.as_str()) {
                tracing::info!("got new token");
                tracing::trace!("token: {token}");

                let mut t = self.token.write().await;
                *t = Some(token.to_string());
                return Ok(token.to_string());
            }

            attempt += 1;
        }

        eyre::bail!("token could not be refreshed");
    }

    #[tracing::instrument(skip(self))]
    async fn fetch(&self, endpoint: &str) -> eyre::Result<serde_json::Value> {
        let token = self.refresh_token(false).await?;

        let csrf = Uuid::new_v4().to_string().replace('-', "");

        let cookies = [
            format!("guest_id_ads=v1%3A{token}"),
            format!("guest_id_marketing=v1%3A{token}"),
            format!("guest_id=v1%3A{token}"),
            format!("ct0=${csrf}"),
        ]
        .into_iter()
        .collect::<Vec<_>>()
        .join("; ");

        let mut headers = Self::headers();
        headers.insert(
            "authorization",
            HeaderValue::from_static(GUEST_BEARER_TOKEN),
        );
        headers.insert("x-csrf-token", HeaderValue::from_str(&csrf)?);
        headers.insert("x-guest-token", HeaderValue::from_str(&token)?);
        headers.insert("x-twitter-active-user", HeaderValue::from_static("yes"));
        headers.insert("cookie", HeaderValue::from_str(&cookies)?);

        tracing::debug!("starting request");

        let resp = self
            .client
            .get(endpoint)
            .headers(headers)
            .send()
            .await?
            .error_for_status()?;

        let rate_limit = resp
            .headers()
            .get("x-rate-limit-remaining")
            .and_then(|limit| limit.to_str().ok()?.parse::<u32>().ok());
        if matches!(rate_limit, Some(rate_limit) if rate_limit < 10) {
            tracing::info!("rate limit low, refreshing token");
            self.refresh_token(true).await?;
        } else if let Some(rate_limit) = rate_limit {
            tracing::debug!(rate_limit, "found rate limit");
        } else {
            tracing::warn!("response had no rate limit remaining");
        }

        Ok(resp.json().await?)
    }

    async fn process_tweet(
        &self,
        id: &str,
        user_id: &str,
        user: &serde_json::Value,
        tweet: &serde_json::Value,
    ) -> eyre::Result<SubmissionResult> {
        let screen_name = user["screen_name"]
            .as_str()
            .context("missing screen name")?;

        let posted_at = tweet["created_at"].as_str().and_then(|created_at| {
            chrono::Utc
                .datetime_from_str(created_at, "%a %b %d %T %z %Y")
                .ok()
        });

        let hashtags: Vec<_> = tweet["entities"]["hashtags"]
            .as_array()
            .map(|hashtags| {
                hashtags
                    .iter()
                    .flat_map(|hashtag| Some(hashtag["text"].as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let rating = if tweet["possibly_sensitive"].as_bool() == Some(true) {
            Some(Rating::Adult)
        } else {
            None
        };

        let mut submission_media = Vec::with_capacity(1);
        if let Some(media) = tweet["extended_entities"]["media"].as_array() {
            for media in media {
                let mut image = process_file(
                    &self.pool,
                    &self.download_path,
                    &self.client,
                    media["id_str"].as_str().map(|id| id.to_string()),
                    media["media_url_https"]
                        .as_str()
                        .context("media url was not string")?,
                )
                .await?;
                image.extra = Some(media.clone());
                submission_media.push(image);
            }
        }

        let result = SubmissionResult::Fetched(Submission {
            site: self.site(),
            submission_id: id.to_string(),
            deleted: false,
            link: format!("https://twitter.com/{screen_name}/status/{id}"),
            posted_at,
            title: None,
            artists: vec![Artist {
                site_artist_id: user_id.to_string(),
                name: screen_name.to_string(),
                link: Some(format!("https://twitter.com/{screen_name}")),
            }],
            tags: hashtags,
            description: tweet["full_text"].as_str().map(|s| s.to_string()),
            rating,
            media: submission_media,
            retrieved_at: Some(chrono::Utc::now()),
            extra: Some(serde_json::json!({
                "tweet": tweet,
                "user_id": user_id,
            })),
        });

        Ok(result)
    }

    pub async fn load_tweet(&self, id: &str) -> eyre::Result<SubmissionResult> {
        let features = serde_json::to_string(&serde_json::json!({
            "creator_subscriptions_tweet_preview_api_enabled": true,
            "tweetypie_unmention_optimization_enabled": true,
            "responsive_web_edit_tweet_api_enabled": true,
            "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
            "view_counts_everywhere_api_enabled": true,
            "longform_notetweets_consumption_enabled": true,
            "responsive_web_twitter_article_tweet_consumption_enabled": false,
            "tweet_awards_web_tipping_enabled": false,
            "freedom_of_speech_not_reach_fetch_enabled": true,
            "standardized_nudges_misinfo": true,
            "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
            "longform_notetweets_rich_text_read_enabled": true,
            "longform_notetweets_inline_media_enabled": true,
            "responsive_web_graphql_exclude_directive_enabled": true,
            "verified_phone_label_enabled": false,
            "responsive_web_media_download_video_enabled": false,
            "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
            "responsive_web_graphql_timeline_navigation_enabled": true,
            "responsive_web_enhance_cards_enabled": false
        }))?;

        let variables = serde_json::to_string(&serde_json::json!({
            "tweetId": id,
            "withCommunity": false,
            "includePromotedContent": false,
            "withVoice": false,
        }))?;

        let url = reqwest::Url::parse_with_params(
            "https://twitter.com/i/api/graphql/3HC_X_wzxnMmUBRIn3MWpQ/TweetResultByRestId",
            &[("variables", &variables), ("features", &features)],
        )?;

        let conversation = self.fetch(url.as_str()).await?;
        tracing::trace!("got conversation: {conversation:?}");

        let tweet = conversation["data"]["tweetResult"]["result"]
            .get("legacy")
            .context("missing tweet object")?;

        let user_id = tweet["user_id_str"]
            .as_str()
            .context("missing user id str")?;
        let user = conversation["data"]["tweetResult"]["result"]["core"]["user_results"]["result"]
            .get("legacy")
            .context("missing user object")?;

        let result = self.process_tweet(id, user_id, user, tweet).await?;

        Ok(result)
    }

    #[tracing::instrument(skip(self))]
    pub async fn load_user_timeline(
        &self,
        user_id: &str,
        last_fetched_id: Option<i64>,
    ) -> eyre::Result<()> {
        const ENDPOINT: &str =
            "https://twitter.com/i/api/graphql/BeHK76TOCY3P8nO-FWocjA/UserTweets";

        let mut tx = self.pool.begin().await?;

        let features = serde_json::to_string(&serde_json::json!({
            "blue_business_profile_image_shape_enabled": false,
            "responsive_web_graphql_exclude_directive_enabled": true,
            "verified_phone_label_enabled": false,
            "responsive_web_graphql_timeline_navigation_enabled": true,
            "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
            "tweetypie_unmention_optimization_enabled": true,
            "vibe_api_enabled": true,
            "responsive_web_edit_tweet_api_enabled": true,
            "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
            "view_counts_everywhere_api_enabled": true,
            "longform_notetweets_consumption_enabled": true,
            "tweet_awards_web_tipping_enabled": false,
            "freedom_of_speech_not_reach_fetch_enabled": false,
            "standardized_nudges_misinfo": true,
            "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": false,
            "interactive_text_enabled": true,
            "responsive_web_text_conversations_enabled": false,
            "longform_notetweets_richtext_consumption_enabled": false,
            "responsive_web_enhance_cards_enabled": false
        }))?;

        let mut latest_id = sqlx::query_scalar!(
            "SELECT latest_id FROM twitter_user WHERE twitter_id = $1",
            user_id.parse::<i64>().unwrap()
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        let mut found_previous_id = false;
        let mut next_cursor = None;

        let mut found_tweets = 0;

        loop {
            tracing::info!("loading page at cursor: {next_cursor:?}");

            let variables = serde_json::to_string(&serde_json::json!({
                "userId": user_id,
                "count": 40,
                "cursor": next_cursor,
                "includePromotedContent": true,
                "withQuickPromoteEligibilityTweetFields": true,
                "withDownvotePerspective": false,
                "withReactionsMetadata": false,
                "withReactionsPerspective": false,
                "withVoice": true,
                "withV2Timeline": true,
            }))?;

            let url = reqwest::Url::parse_with_params(
                ENDPOINT,
                &[("variables", &variables), ("features", &features)],
            )?;

            let timeline = self.fetch(url.as_str()).await?;

            let entries: Vec<_> = timeline["data"]["user"]["result"]["timeline_v2"]["timeline"]
                ["instructions"]
                .as_array()
                .map(|instructions| {
                    instructions
                        .iter()
                        .filter_map(|instruction| instruction["entries"].as_array())
                        .flatten()
                })
                .context("missing entries")?
                .collect();

            next_cursor = entries
                .iter()
                .find(|entry| {
                    entry["content"]["entryType"] == "TimelineTimelineCursor"
                        && entry["content"]["cursorType"] == "Bottom"
                })
                .and_then(|entry| entry["content"]["value"].as_str())
                .map(|cursor| cursor.to_string());

            let mut had_tweet = false;
            for entry in entries.iter() {
                let content = &entry["content"];

                if content["entryType"].as_str().unwrap_or_default() != "TimelineTimelineItem"
                    || content["itemContent"]["itemType"]
                        .as_str()
                        .unwrap_or_default()
                        != "TimelineTweet"
                {
                    tracing::trace!("not timeline tweet");
                    continue;
                }

                let result = &content["itemContent"]["tweet_results"]["result"];
                let user = &result["core"]["user_results"]["result"]["legacy"];
                let tweet = &result["legacy"];

                if result.is_null() {
                    tracing::warn!("result was null");
                    continue;
                }

                found_tweets += 1;

                let tweet_id = result["rest_id"]
                    .as_str()
                    .context("missing tweet rest id")?;
                if let Ok(id) = tweet_id.parse() {
                    if Some(id) > latest_id {
                        tracing::trace!(id, "found greater id");
                        latest_id = Some(id);
                    }

                    if let Some(last_id) = last_fetched_id {
                        if id == last_id {
                            tracing::info!(last_id, "found previous highest id");
                            found_previous_id = true;
                        }
                    }
                }
                had_tweet = true;

                if tweet["retweeted"].as_bool().unwrap_or_default() {
                    tracing::trace!(tweet_id, "was retweeted");
                    continue;
                }

                if tweet["in_reply_to_user_id_str"] != user_id {
                    tracing::trace!(tweet_id, "in reply to other user");
                    continue;
                }

                let result = self.process_tweet(tweet_id, user_id, user, tweet).await?;
                if matches!(&result, SubmissionResult::Fetched(Submission { media, .. }) if media.is_empty())
                {
                    tracing::trace!(tweet_id, "tweet had no media");
                    continue;
                }

                if let SubmissionResult::Fetched(sub) = &result {
                    super::insert_submission(&self.pool, sub).await?;
                }
            }

            if found_previous_id || next_cursor.is_none() || !had_tweet {
                tracing::info!("found previous id or cursor was none");
                break;
            }
        }

        tracing::debug!(found_tweets, "completed fetch");

        if let Some(latest_id) = latest_id {
            sqlx::query!("UPDATE twitter_user SET latest_id = $2, updated_at = current_timestamp WHERE twitter_id = $1", user_id.parse::<i64>().unwrap(), latest_id).execute(&mut tx).await?;
        }

        tx.commit().await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn resolve_screen_name(&self, screen_name: &str) -> eyre::Result<String> {
        const ENDPOINT: &str =
            "https://twitter.com/i/api/graphql/sLVLhk0bGj3MVFEKTdax1w/UserByScreenName";

        let features = serde_json::to_string(&serde_json::json!({
          "blue_business_profile_image_shape_enabled": false,
          "responsive_web_graphql_exclude_directive_enabled": true,
          "verified_phone_label_enabled": false,
          "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
          "responsive_web_graphql_timeline_navigation_enabled": true
        }))?;

        let variables = serde_json::to_string(&serde_json::json!({
            "screen_name": screen_name,
            "withSafetyModeUserFields": true,
        }))?;

        let url = reqwest::Url::parse_with_params(
            ENDPOINT,
            &[("variables", &variables), ("features", &features)],
        )?;

        let user = self.fetch(url.as_str()).await?;
        let id = user["data"]["user"]["result"]["rest_id"]
            .as_str()
            .context("missing rest id")?;

        sqlx::query!("INSERT INTO twitter_user (twitter_id, data) VALUES ($1, $2) ON CONFLICT (twitter_id) DO UPDATE SET data = EXCLUDED.data", id.parse::<i64>().unwrap(), user["data"]["user"]["result"]).execute(&self.pool).await?;

        Ok(id.to_string())
    }
}

#[async_trait]
impl LoadableSite for Twitter {
    fn site(&self) -> Site {
        Site::Twitter
    }

    #[tracing::instrument(skip(self))]
    async fn load_multiple(&self, ids: Vec<&str>) -> eyre::Result<Vec<SubmissionResult>> {
        tracing::info!("starting to load submissions");

        futures::stream::iter(ids)
            .then(|id| self.load_tweet(id))
            .try_collect()
            .await
    }
}
