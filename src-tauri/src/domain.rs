use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub default_output_dir: String,
    pub default_arch: String,
    pub default_mirror: String,
    pub download_workers: u32,
    pub last_package_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub default_output_dir: Option<String>,
    pub default_arch: Option<String>,
    pub default_mirror: Option<String>,
    pub download_workers: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_patch")]
    pub last_package_name: Option<Option<String>>,
}

impl AppSettings {
    pub fn apply_patch(&mut self, patch: SettingsPatch) {
        if let Some(value) = patch.default_output_dir {
            self.default_output_dir = value;
        }
        if let Some(value) = patch.default_arch {
            self.default_arch = value;
        }
        if let Some(value) = patch.default_mirror {
            self.default_mirror = value;
        }
        if let Some(value) = patch.download_workers {
            self.download_workers = value;
        }
        if let Some(value) = patch.last_package_name {
            self.last_package_name = value;
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mirror {
    pub id: String,
    pub name: String,
    pub registry: String,
    pub is_default: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl fmt::Debug for Mirror {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mirror")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("registry", &self.registry)
            .field("is_default", &self.is_default)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorInput {
    pub name: String,
    pub registry: String,
    pub is_default: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub fn normalize_registry(value: &str) -> String {
    let normalized = value.trim().trim_end_matches('/').to_ascii_lowercase();
    let without_scheme = normalized
        .strip_prefix("https://")
        .or_else(|| normalized.strip_prefix("http://"))
        .unwrap_or(&normalized);
    without_scheme.to_owned()
}

impl fmt::Debug for MirrorInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MirrorInput")
            .field("name", &self.name)
            .field("registry", &self.registry)
            .field("is_default", &self.is_default)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPatch {
    pub name: Option<String>,
    pub registry: Option<String>,
    pub is_default: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_patch")]
    pub username: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_patch")]
    pub password: Option<Option<String>>,
}

impl fmt::Debug for MirrorPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MirrorPatch")
            .field("name", &self.name)
            .field("registry", &self.registry)
            .field("is_default", &self.is_default)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStartRequest {
    pub package_name: String,
    pub tag: String,
    pub arch: String,
    pub mirror: String,
    pub output_dir: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchStartRequest {
    pub images: Vec<DownloadStartRequest>,
    pub max_concurrent_images: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Queued,
    Running,
    Cancelling,
    Completed,
    PartialFailed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchTaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchTaskSnapshot {
    pub task_id: String,
    pub image: ImageReference,
    pub status: BatchTaskStatus,
    pub download: DownloadSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSnapshot {
    pub batch_id: String,
    pub status: BatchStatus,
    pub max_concurrent_images: u32,
    pub tasks: Vec<BatchTaskSnapshot>,
}

impl fmt::Debug for DownloadStartRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadStartRequest")
            .field("package_name", &self.package_name)
            .field("tag", &self.tag)
            .field("arch", &self.arch)
            .field("mirror", &self.mirror)
            .field("output_dir", &self.output_dir)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownloadStatus {
    Idle,
    Preparing,
    Downloading,
    Verifying,
    Packing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayerStatus {
    Waiting,
    Downloading,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerProgress {
    pub digest: String,
    pub size: u64,
    pub status: LayerStatus,
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub current_layer: u32,
    pub total_layers: u32,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub speed: f64,
    pub layers: Vec<LayerProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedDownload {
    pub output_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSnapshot {
    pub task_id: String,
    pub image: ImageReference,
    pub status: DownloadStatus,
    pub progress: DownloadProgress,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: String,
    pub package_name: String,
    pub tag: String,
    pub arch: String,
    pub mirror: String,
    pub full_image: String,
    pub output_path: Option<String>,
    pub size: Option<String>,
    pub status: DownloadStatus,
    pub downloaded_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    pub level: String,
    pub message: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedImage {
    pub registry: String,
    pub repository: String,
    pub reference: String,
    pub manifest_digest: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    pub platform: Platform,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeMetadata {
    pub version: u32,
    pub image: ImageReference,
    pub platform: Platform,
    pub manifest_digest: String,
    pub descriptor: Descriptor,
    pub etag: Option<String>,
    pub downloaded: u64,
}

fn deserialize_optional_patch<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
