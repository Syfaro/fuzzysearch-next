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
    /// A SHA256 of the contents of the image, if known.
    pub sha256: Option<String>,

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
