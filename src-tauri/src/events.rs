use crate::domain::{DownloadProgress, LogEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DOWNLOAD_STATE_EVENT: &str = "download://state";
pub const DOWNLOAD_PROGRESS_EVENT: &str = "download://progress";
pub const DOWNLOAD_LOG_EVENT: &str = "download://log";
pub const BATCH_STATE_EVENT: &str = "batch://state";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgressEvent {
    pub task_id: String,
    #[serde(flatten)]
    pub progress: DownloadProgress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLogEvent {
    pub task_id: String,
    #[serde(flatten)]
    pub log: LogEvent,
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: &'static str, payload: Value);
}
