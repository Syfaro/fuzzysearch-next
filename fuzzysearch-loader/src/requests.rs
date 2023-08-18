//! Receive and respond to incoming requests to fetch information about submissions.

use std::{collections::HashMap, sync::Arc};

use async_nats::Message;
use bytes::Bytes;
use fuzzysearch_common::{
    FetchPolicy, FetchRequest, FetchResponse, FetchStatus, FetchedSubmission,
    FetchedSubmissionData, Site, Submission, SubmissionQuery,
};
use serde::Serialize;
use sqlx::PgPool;
use tap::TapFallible;
use tracing::Instrument;

use crate::sites::{BoxSite, LoadSubmissions, SubmissionResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchReason {
    #[serde(rename = "demand")]
    OnDemand,
    Live,
}

async fn fetch_existing_submissions(
    pool: &PgPool,
    ids: &[(Site, String)],
) -> Result<HashMap<(Site, String), Submission>, async_nats::service::error::Error> {
    let input: Vec<_> = ids
        .iter()
        .map(|(site, id)| {
            serde_json::json!({
                "site": site.to_string(),
                "site_submission_id": id,
            })
        })
        .collect();

    let submissions = sqlx::query_file_as!(
        crate::sites::DbSubmission,
        "queries/submission_lookup_id.sql",
        serde_json::Value::Array(input)
    )
    .fetch_all(pool)
    .await
    .map_err(|err| async_nats::service::error::Error {
        status: err.to_string(),
        code: 503,
    })?;

    submissions
        .into_iter()
        .map(|submission| {
            let site = serde_plain::from_str(&submission.site)?;
            let rating = submission
                .rating
                .map(|rating| serde_plain::from_str(&rating))
                .transpose()?;

            let key = (site, submission.site_submission_id.clone());

            let submission = Submission {
                id: Some(submission.id),
                site,
                submission_id: submission.site_submission_id,
                deleted: submission.deleted,
                posted_at: submission.posted_at,
                link: submission.link,
                title: submission.title,
                artists: submission
                    .artists
                    .map(|artists| artists.0)
                    .unwrap_or_default(),
                tags: submission.tags.unwrap_or_default(),
                description: submission.description,
                rating,
                media: submission
                    .media
                    .map(|artists| artists.0)
                    .unwrap_or_default(),
                retrieved_at: submission.retrieved_at,
                extra: submission.extra,
            };

            Ok((key, submission))
        })
        .collect::<eyre::Result<HashMap<_, _>>>()
        .map_err(|err| async_nats::service::error::Error {
            status: err.to_string(),
            code: 503,
        })
}

#[tracing::instrument(skip_all)]
pub async fn handle_fetch(
    pool: PgPool,
    nats: async_nats::Client,
    sites: Arc<Vec<BoxSite>>,
    message: &Message,
) -> Result<Bytes, async_nats::service::error::Error> {
    let req: FetchRequest = serde_json::from_slice(&message.payload).map_err(|err| {
        async_nats::service::error::Error {
            status: format!("cannot deserialize request: {err}"),
            code: 400,
        }
    })?;

    let submission_ids = match req.query {
        SubmissionQuery::SubmissionId { submission_ids } => submission_ids,
        SubmissionQuery::PerceptualHash { hashes } => {
            let hashes: Vec<_> = hashes.into_iter().map(i64::from_be_bytes).collect();

            sqlx::query_file!("queries/submission_lookup_perceptual_gradient.sql", &hashes)
                .fetch_all(&pool)
                .await
                .map_err(|err| async_nats::service::error::Error {
                    status: err.to_string(),
                    code: 503,
                })?
                .into_iter()
                .flat_map(|row| {
                    let site = serde_plain::from_str(&row.name)
                        .tap_err(|err| tracing::error!("could not deserialize site name: {err}"))
                        .ok()?;
                    Some((site, row.site_submission_id))
                })
                .collect()
        }
    };

    if submission_ids.len() > 25 {
        return Err(async_nats::service::error::Error {
            status: "request had too many submissions".to_string(),
            code: 418,
        });
    }

    let mut ready_submissions: HashMap<(Site, String), FetchedSubmission> =
        HashMap::with_capacity(submission_ids.len());

    tracing::debug!("request policy: {:?}", req.policy);

    if matches!(req.policy, FetchPolicy::Never | FetchPolicy::Maybe { .. }) {
        tracing::debug!("looking for cached values");

        let submissions = fetch_existing_submissions(&pool, &submission_ids).await?;
        tracing::debug!(len = submissions.len(), "found cached submissions");

        ready_submissions.extend(
            submissions
                .into_iter()
                .filter(|(key, submission)| match req.policy {
                    FetchPolicy::Never => {
                        tracing::debug!(?key, "policy was never fetch, using cached value");
                        true
                    }
                    FetchPolicy::Maybe { older_than, .. }
                        if submission.retrieved_at >= Some(older_than) =>
                    {
                        tracing::debug!(
                            ?key,
                            "policy was maybe fetch and was retreived recently enough"
                        );
                        tracing::trace!("retrieved_at: {:?}", submission.retrieved_at);
                        tracing::trace!("older_than: {older_than:?}");
                        true
                    }
                    _ => {
                        tracing::info!(?key, "policy did not permit using cached value");
                        false
                    }
                })
                .map(|(key, submission)| {
                    (
                        key.clone(),
                        FetchedSubmission {
                            site: key.0,
                            submission_id: key.1,
                            submission: FetchedSubmissionData::Success {
                                fetch_status: FetchStatus::Cached,
                                submission,
                            },
                        },
                    )
                }),
        );
    }

    let needing_load: Vec<_> = submission_ids
        .iter()
        .cloned()
        .filter(|id| !ready_submissions.contains_key(id))
        .collect();
    tracing::debug!("submissions still needing load: {needing_load:?}");

    if !needing_load.is_empty() && !matches!(req.policy, FetchPolicy::Never) {
        tracing::info!(len = needing_load.len(), "loading submissions");

        let task = tokio::spawn(
            async move {
                let sites = sites.as_slice();
                let res = sites.load_submissions(&needing_load).await;
                tracing::info!("completed submission load");
                res
            }
            .in_current_span(),
        );

        let submissions = if let Some(timeout) = req.timeout {
            match tokio::time::timeout(timeout, task).await {
                Ok(res) => res,
                Err(_err) => {
                    tracing::warn!("timeout elapsed");
                    Ok(Ok(vec![]))
                }
            }
        } else {
            task.await
        }
        .map_err(|err| async_nats::service::error::Error {
            status: err.to_string(),
            code: 503,
        })?;

        let submissions = submissions
            .map_err(|err| async_nats::service::error::Error {
                status: err.to_string(),
                code: 503,
            })?
            .into_iter()
            .map(|submission| match submission {
                SubmissionResult::Fetched(submission) => (
                    (submission.site, submission.submission_id.clone()),
                    FetchedSubmission {
                        site: submission.site,
                        submission_id: submission.submission_id.clone(),
                        submission: FetchedSubmissionData::Success {
                            fetch_status: FetchStatus::Fetched,
                            submission,
                        },
                    },
                ),
                SubmissionResult::Error {
                    site,
                    submission_id,
                    message,
                } => (
                    (site, submission_id.clone()),
                    FetchedSubmission {
                        site,
                        submission_id,
                        submission: FetchedSubmissionData::Error { message },
                    },
                ),
            });

        ready_submissions.extend(submissions);
    }

    let needing_load: Vec<_> = submission_ids
        .iter()
        .cloned()
        .filter(|id| !ready_submissions.contains_key(id))
        .collect();
    tracing::debug!("submissions still needing load: {needing_load:?}");

    if !needing_load.is_empty()
        && matches!(
            req.policy,
            FetchPolicy::Always { return_stale: true }
                | FetchPolicy::Maybe {
                    return_stale: true,
                    ..
                }
        )
    {
        tracing::warn!("still needed submissions and permits stale, loading");

        let submissions = fetch_existing_submissions(&pool, &needing_load).await?;
        tracing::info!(len = submissions.len(), "found cached submissions");

        ready_submissions.extend(submissions.into_iter().map(|(key, submission)| {
            (
                key.clone(),
                FetchedSubmission {
                    site: key.0,
                    submission_id: key.1,
                    submission: FetchedSubmissionData::Success {
                        fetch_status: FetchStatus::Cached,
                        submission,
                    },
                },
            )
        }));
    }

    for (key, sub) in ready_submissions.iter_mut() {
        match sub {
            FetchedSubmission {
                submission:
                    FetchedSubmissionData::Success {
                        fetch_status: FetchStatus::Fetched,
                        submission,
                    },
                ..
            } => {
                if let Err(err) =
                    crate::sites::insert_submission(&pool, &nats, FetchReason::OnDemand, submission)
                        .await
                {
                    tracing::error!(?key, "could not insert submission: {err}");
                }
            }
            _ => tracing::trace!(?key, "ignoring submission that wasn't newly fetched"),
        }
    }

    let submissions: Vec<_> = ready_submissions.into_values().collect();
    tracing::info!(len = submissions.len(), "prepared results for request");

    let resp = FetchResponse { submissions };

    Ok(Bytes::from(serde_json::to_vec(&resp).map_err(|err| {
        async_nats::service::error::Error {
            status: format!("could not serialize response: {err}"),
            code: 503,
        }
    })?))
}
