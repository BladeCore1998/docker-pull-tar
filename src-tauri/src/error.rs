use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("a download task is already running")]
    TaskAlreadyRunning,
    #[error("invalid image reference: {0}")]
    InvalidImageReference(String),
    #[error("registry authentication failed")]
    RegistryAuthFailed,
    #[error("registry resource was not found")]
    RegistryNotFound,
    #[error("requested platform was not found")]
    PlatformNotFound,
    #[error("TLS certificate validation failed")]
    TlsValidationFailed,
    #[error("network request timed out")]
    NetworkTimeout,
    #[error("the registry does not support resuming this download")]
    ResumeNotSupported,
    #[error("downloaded content did not match its expected digest")]
    DigestMismatch,
    #[error("unsupported layer media type")]
    UnsupportedLayerMediaType,
    #[error("output location is not writable")]
    OutputNotWritable,
    #[error("insufficient disk space")]
    DiskSpaceInsufficient,
    #[error("download task was cancelled")]
    TaskCancelled,
    #[error("configuration is corrupted")]
    ConfigCorrupted,
    #[error("mirror configuration is invalid")]
    InvalidMirror,
    #[error("mirror was not found")]
    MirrorNotFound,
    #[error("mirror conflicts with an existing mirror")]
    MirrorConflict,
    #[error("another default mirror is required")]
    DefaultMirrorRequired,
    #[error("credential storage operation failed")]
    CredentialStoreFailed,
    #[error("configuration storage operation failed")]
    ConfigIoFailed,
    #[error("configuration commit result is indeterminate")]
    ConfigCommitIndeterminate,
    #[error("internal error: {0}")]
    InternalError(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TaskAlreadyRunning => "TASK_ALREADY_RUNNING",
            Self::InvalidImageReference(_) => "INVALID_IMAGE_REFERENCE",
            Self::RegistryAuthFailed => "REGISTRY_AUTH_FAILED",
            Self::RegistryNotFound => "REGISTRY_NOT_FOUND",
            Self::PlatformNotFound => "PLATFORM_NOT_FOUND",
            Self::TlsValidationFailed => "TLS_VALIDATION_FAILED",
            Self::NetworkTimeout => "NETWORK_TIMEOUT",
            Self::ResumeNotSupported => "RESUME_NOT_SUPPORTED",
            Self::DigestMismatch => "DIGEST_MISMATCH",
            Self::UnsupportedLayerMediaType => "UNSUPPORTED_LAYER_MEDIA_TYPE",
            Self::OutputNotWritable => "OUTPUT_NOT_WRITABLE",
            Self::DiskSpaceInsufficient => "DISK_SPACE_INSUFFICIENT",
            Self::TaskCancelled => "TASK_CANCELLED",
            Self::ConfigCorrupted => "CONFIG_CORRUPTED",
            Self::InvalidMirror => "INVALID_MIRROR",
            Self::MirrorNotFound => "MIRROR_NOT_FOUND",
            Self::MirrorConflict => "MIRROR_CONFLICT",
            Self::DefaultMirrorRequired => "DEFAULT_MIRROR_REQUIRED",
            Self::CredentialStoreFailed => "CREDENTIAL_STORE_FAILED",
            Self::ConfigIoFailed => "CONFIG_IO_FAILED",
            Self::ConfigCommitIndeterminate => "CONFIG_COMMIT_INDETERMINATE",
            Self::InternalError(_) => "INTERNAL_ERROR",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::NetworkTimeout)
    }

    fn details(&self) -> BTreeMap<String, String> {
        let mut details = BTreeMap::new();
        if let Self::InvalidImageReference(input) = self {
            details.insert("input".into(), input.clone());
        }
        details
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub details: BTreeMap<String, String>,
}

impl From<AppError> for CommandError {
    fn from(error: AppError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
            retryable: error.retryable(),
            details: error.details(),
        }
    }
}
