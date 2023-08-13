use serde::{Deserialize, Serialize};

#[cfg(feature = "api-types")]
use utoipa::ToSchema;

/// A service that FuzzySearch supports.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Twitter,
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Twitter => write!(f, "Twitter"),
        }
    }
}

/// The rating of a given submission.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    General,
    Mature,
    Adult,
}

impl std::str::FromStr for Rating {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rating = match s {
            "g" | "s" | "general" => Rating::General,
            "m" | "q" | "mature" => Rating::Mature,
            "a" | "e" | "adult" | "explicit" => Rating::Adult,
            _ => return Err("unknown rating"),
        };

        Ok(rating)
    }
}

/// An image upload.
#[allow(dead_code)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct Image {
    /// Image to search.
    #[cfg_attr(feature = "api-types", schema(value_type = String, format = Binary))]
    pub image: Vec<u8>,
    /// Maximum distance for search results.
    pub distance: Option<u64>,
}

/// Site-specific information.
#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
#[serde(tag = "site", content = "site_info")]
pub enum SiteInfo {
    FurAffinity {
        /// The file ID on FurAffinity. Note that this is not the same as the submission ID.
        file_id: i32,
    },
    Weasyl,
    Twitter,
    #[serde(rename = "e621")]
    E621 {
        /// The sources attached to this post.
        sources: Vec<String>,
    },
    #[default]
    Unknown,
}

/// An image search result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct SearchResult {
    /// The ID of the submission on the site.
    pub site_id: i64,
    /// The ID of the submission on the site, as a string.
    pub site_id_str: String,
    /// The URL of the image.
    pub url: String,
    /// The name of the file.
    pub filename: String,
    /// The artists associated with the image.
    pub artists: Option<Vec<String>>,
    /// The rating of the image.
    pub rating: Option<Rating>,
    /// When the image was posted, if known.
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Any tags associated with the image.
    pub tags: Vec<String>,
    /// A SHA256 of the contents of the image, if known.
    pub sha256: Option<String>,

    /// If the submission appears to be deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    /// When the submission was retrieved for the search index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved_at: Option<chrono::DateTime<chrono::Utc>>,

    /// The hash of the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<i64>,
    /// The hash of the image, as a string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash_str: Option<String>,
    /// The distance between the searched hash and the hash of this image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<i64>,
    /// The searched hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_hash: Option<i64>,
    /// The searched hash, as a string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_hash_str: Option<String>,

    /// Site-specific information.
    #[serde(flatten)]
    pub site_info: SiteInfo,
}

impl SearchResult {
    pub fn site_name(&self) -> &'static str {
        match &self.site_info {
            SiteInfo::FurAffinity { .. } => "FurAffinity",
            SiteInfo::E621 { .. } => "e621",
            SiteInfo::Weasyl => "Weasyl",
            SiteInfo::Twitter => "Twitter",
            SiteInfo::Unknown => "Unknown",
        }
    }

    pub fn site(&self) -> Site {
        match &self.site_info {
            SiteInfo::FurAffinity { .. } => Site::FurAffinity,
            SiteInfo::E621 { .. } => Site::E621,
            SiteInfo::Weasyl => Site::Weasyl,
            SiteInfo::Twitter => Site::Twitter,
            _ => panic!("unknown site"),
        }
    }

    pub fn url(&self) -> String {
        match self.site_info {
            SiteInfo::FurAffinity { .. } => {
                format!("https://www.furaffinity.net/view/{}/", self.site_id)
            }
            SiteInfo::E621 { .. } => format!("https://e621.net/posts/{}", self.site_id),
            SiteInfo::Weasyl => format!("https://www.weasyl.com/view/{}/", self.site_id),
            SiteInfo::Twitter => format!(
                "https://twitter.com/{}/status/{}",
                self.artists
                    .as_ref()
                    .and_then(|artists| artists.first().map(String::as_str))
                    .unwrap_or_default(),
                self.site_id
            ),
            SiteInfo::Unknown => format!("unknown-{}", self.site_id),
        }
    }

    pub fn id(&self) -> String {
        format!("{}-{}", self.site_name(), self.site_id)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct FurAffinityFile {
    pub id: i32,
    pub file_id: Option<i32>,
    pub artist: Option<String>,
    pub hash: Option<i64>,
    pub hash_str: Option<String>,
    pub url: Option<String>,
    pub filename: Option<String>,
    pub rating: Option<Rating>,
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub file_size: Option<i32>,
    pub sha256: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub tags: Vec<String>,
}

/// A site that the loader can fetch information from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub enum Site {
    #[serde(rename = "e621")]
    E621,
    FurAffinity,
    Twitter,
    Weasyl,
}

/// An artist on a site.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct Artist {
    /// An ID for the artist on the site. Often just their username.
    #[serde(skip)]
    pub site_artist_id: String,

    /// The artist's name.
    pub name: String,
    /// A link to the artist's page, if one exists.
    pub link: Option<String>,
}

/// A submission.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct Submission {
    /// An ID uniquely identifying this submission revision within FuzzySearch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<uuid::Uuid>,
    /// The site where a submission resides.
    pub site: Site,
    /// The ID of the submission, as given by the site.
    pub submission_id: String,
    /// If the submission was deleted.
    pub deleted: bool,
    /// When the submission was posted.
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// A link to the submission on the site.
    pub link: String,
    /// The title of the submission.
    pub title: Option<String>,
    /// The artists responsible for this submission.
    pub artists: Vec<Artist>,
    /// The tags on the submission.
    pub tags: Vec<String>,
    /// A description of the submission.
    pub description: Option<String>,
    /// The content rating of the submission.
    pub rating: Option<Rating>,
    /// Any media files associated with the submission.
    pub media: Vec<Media>,
    /// When the submission data was retrieved from the site.
    pub retrieved_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Arbitrary extra data about the submission.
    #[cfg_attr(feature = "api-types", schema(value_type = Object))]
    pub extra: Option<serde_json::Value>,
}

/// Media from a submission.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct Media {
    /// An ID for this media related to the submission.
    ///
    /// Not all sites use this.
    pub site_id: Option<String>,
    /// If the media was deleted.
    pub deleted: bool,
    /// The URL to this media.
    pub url: Option<String>,
    /// The SHA256 hash of the file.
    pub file_sha256: Option<Vec<u8>>,
    /// The size of the file.
    pub file_size: Option<i64>,
    /// The mime type of the file.
    pub mime_type: Option<String>,
    /// Any processed frames of this media.
    pub frames: Vec<MediaFrame>,
    /// Arbitrary extra data for this media on the related submission.
    #[cfg_attr(feature = "api-types", schema(value_type = Object))]
    pub extra: Option<serde_json::Value>,
}

/// A processed frame of some media.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub struct MediaFrame {
    /// The index of the frame.
    pub frame_index: i64,
    /// A perceptual gradient hash of the frame.
    pub perceptual_gradient: Option<[u8; 8]>,
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::E621 => "e621",
            Self::FurAffinity => "FurAffinity",
            Self::Twitter => "Twitter",
            Self::Weasyl => "Weasyl",
        };

        write!(f, "{name}")
    }
}

/// A request to load data from a site.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FetchRequest {
    /// Selection for which submissions to load.
    pub query: SubmissionQuery,
    /// The policy to use for fetching submissions.
    #[serde(default)]
    pub policy: FetchPolicy,
}

/// The way to identify submissions to load.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionQuery {
    /// Load submissions for a site with the given IDs.
    SubmissionId {
        /// The site and ID of each submission to load.
        submission_ids: Vec<(Site, String)>,
    },
    /// Load submissions that have a specific perceptual hash.
    PerceptualHash {
        /// The perceptual hashes to load.
        hashes: Vec<[u8; 8]>,
    },
}

/// The policy to use for fetching new data.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchPolicy {
    /// Never fetch new data, only return what already is saved.
    #[default]
    Never,
    /// Maybe fetch new data, if it's older than a given time.
    Maybe {
        /// Only fetch if previous was happened before this date.
        older_than: chrono::DateTime<chrono::Utc>,
        /// If stale data should be returned if it cannot be fetched.
        #[serde(default)]
        return_stale: bool,
    },
    /// Always fetch live data from the site.
    ///
    /// Prefer to use `Maybe` with a short date instead.
    Always {
        /// If stale data should be returned if it cannot be fetched.
        #[serde(default)]
        return_stale: bool,
    },
}

/// A response from attempting to load data from a site.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FetchResponse {
    /// The fetched submissions.
    pub submissions: Vec<FetchedSubmission>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum FetchedSubmission {
    // The submission was successfully loaded.
    Success {
        /// How the submission was fetched.
        fetch_status: FetchStatus,
        /// The submission data.
        submission: Submission,
    },
    /// An error occured loading this submission.
    Error {
        /// The site for the requested submission.
        site: Site,
        /// The ID of the requested submission.
        submission_id: String,
        /// A message about why it failed to load.
        message: Option<String>,
    },
}

/// How the data was actually fetched.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "api-types", derive(ToSchema))]
pub enum FetchStatus {
    /// The value was fetched from the remote.
    Fetched,
    /// The value was retrieved from a cache.
    Cached,
}
