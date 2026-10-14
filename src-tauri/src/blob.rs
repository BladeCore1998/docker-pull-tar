use crate::domain::{Descriptor, ImageReference, Platform, ResumeMetadata};
use crate::error::AppError;
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE},
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const METADATA_VERSION: u32 = 1;

#[derive(Clone)]
pub struct BlobRequest {
    pub url: Url,
    pub image: ImageReference,
    pub platform: Platform,
    pub manifest_digest: String,
    pub descriptor: Descriptor,
    pub authorization: Option<String>,
}

impl fmt::Debug for BlobRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobRequest")
            .field("url", &self.url)
            .field("image", &self.image)
            .field("platform", &self.platform)
            .field("manifest_digest", &self.manifest_digest)
            .field("descriptor", &self.descriptor)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlobProgress {
    pub downloaded: u64,
    pub total: u64,
    pub network_bytes: u64,
    pub network_speed: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlobEvent {
    Progress(BlobProgress),
    Verifying,
}

struct SpeedWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedWindow {
    const SPAN: Duration = Duration::from_secs(1);

    fn new(started: Instant) -> Self {
        Self {
            samples: VecDeque::from([(started, 0)]),
        }
    }

    fn record(&mut self, now: Instant, total_bytes: u64) -> f64 {
        if self.samples.back().is_some_and(|(at, _)| *at == now) {
            self.samples.back_mut().unwrap().1 = total_bytes;
        } else {
            self.samples.push_back((now, total_bytes));
        }
        while self.samples.len() > 1
            && self
                .samples
                .get(1)
                .is_some_and(|(at, _)| now.duration_since(*at) >= Self::SPAN)
        {
            self.samples.pop_front();
        }
        let (started, baseline) = self.samples.front().copied().unwrap_or((now, total_bytes));
        let elapsed = now.duration_since(started);
        if elapsed.is_zero() {
            0.0
        } else {
            total_bytes.saturating_sub(baseline) as f64 / elapsed.as_secs_f64()
        }
    }
}

#[derive(Clone)]
pub struct BlobDownloader {
    client: Client,
    root: PathBuf,
}

impl BlobDownloader {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, AppError> {
        let client = Client::builder().build().map_err(map_request_error)?;
        Ok(Self {
            client,
            root: root.as_ref().to_owned(),
        })
    }

    pub async fn download<F>(
        &self,
        request: BlobRequest,
        cancellation: &CancellationToken,
        mut progress: F,
    ) -> Result<PathBuf, AppError>
    where
        F: FnMut(BlobProgress),
    {
        self.download_with_events(request, cancellation, |event| {
            if let BlobEvent::Progress(update) = event {
                progress(update);
            }
        })
        .await
    }

    pub async fn download_with_events<F>(
        &self,
        request: BlobRequest,
        cancellation: &CancellationToken,
        mut event: F,
    ) -> Result<PathBuf, AppError>
    where
        F: FnMut(BlobEvent),
    {
        let hex = digest_hex(&request.descriptor.digest)?;
        let work = self.root.join(".docker-pull-tar");
        let blobs = self.root.join("blobs");
        tokio::fs::create_dir_all(&work).await.map_err(map_io)?;
        tokio::fs::create_dir_all(&blobs).await.map_err(map_io)?;
        let part = work.join(format!("sha256_{hex}.part"));
        let metadata_path = work.join(format!("sha256_{hex}.resume.json"));
        let target = blobs.join(format!("sha256_{hex}"));

        if tokio::fs::metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.len() == request.descriptor.size)
        {
            event(BlobEvent::Verifying);
            if file_is_verified(&target, request.descriptor.size, &hex, cancellation).await? {
                return Ok(target);
            }
        }

        let expected = ResumeMetadata {
            version: METADATA_VERSION,
            image: normalize_image(&request.image),
            platform: request.platform.clone(),
            manifest_digest: request.manifest_digest.clone(),
            descriptor: request.descriptor.clone(),
            etag: None,
            downloaded: 0,
        };
        let stored = read_metadata(&metadata_path).await;
        let mut resume_etag = None;
        let mut offset = match stored.as_ref() {
            Some(meta) if metadata_matches(meta, &expected) => {
                match tokio::fs::metadata(&part).await {
                    Ok(file) if file.len() <= request.descriptor.size => {
                        resume_etag = meta.etag.clone();
                        file.len()
                    }
                    _ => {
                        remove_if_exists(&part).await?;
                        remove_if_exists(&metadata_path).await?;
                        0
                    }
                }
            }
            _ => {
                remove_if_exists(&part).await?;
                remove_if_exists(&metadata_path).await?;
                0
            }
        };
        let mut hasher = Sha256::new();
        if offset > 0 {
            hash_prefix(&part, offset, &mut hasher, cancellation).await?;
        }
        let mut speed_window = SpeedWindow::new(Instant::now());
        let mut network_bytes = 0u64;
        let mut restarted_after_416 = false;

        if request.descriptor.size == 0 {
            let file = tokio::fs::File::create(&part).await.map_err(map_io)?;
            file.sync_all().await.map_err(map_io)?;
        }

        while offset < request.descriptor.size || offset > 0 {
            let mut builder = self.client.get(request.url.clone());
            if let Some(value) = &request.authorization {
                builder = builder.header(AUTHORIZATION, value);
            }
            if offset > 0 {
                builder = builder.header(RANGE, format!("bytes={offset}-"));
                if let Some(etag) = resume_etag.as_deref() {
                    builder = builder.header(IF_RANGE, etag);
                }
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::TaskCancelled),
                response = builder.send() => response.map_err(map_request_error)?,
            };
            let status = response.status();
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                event(BlobEvent::Verifying);
                if file_is_verified(&part, request.descriptor.size, &hex, cancellation).await? {
                    promote(&part, &target, cancellation).await?;
                    cleanup_after_commit(&metadata_path).await;
                    return Ok(target);
                }
                if restarted_after_416 {
                    return Err(AppError::ResumeNotSupported);
                }
                remove_if_exists(&part).await?;
                remove_if_exists(&metadata_path).await?;
                offset = 0;
                resume_etag = None;
                hasher = Sha256::new();
                restarted_after_416 = true;
                continue;
            }
            if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
                return Err(map_status(status));
            }

            let response_start = if status == StatusCode::PARTIAL_CONTENT {
                let range = validate_content_range(
                    response.headers().get(CONTENT_RANGE),
                    offset,
                    request.descriptor.size,
                )?;
                validate_content_length(response.headers().get(CONTENT_LENGTH), range.len())?;
                Some(range)
            } else {
                if offset > 0 {
                    offset = 0;
                    hasher = Sha256::new();
                }
                validate_content_length(
                    response.headers().get(CONTENT_LENGTH),
                    request.descriptor.size,
                )?;
                None
            };
            let response_etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if status == StatusCode::OK || response_etag.is_some() {
                resume_etag = response_etag;
            }
            let mut metadata = ResumeMetadata {
                etag: resume_etag.clone(),
                downloaded: offset,
                ..expected.clone()
            };
            write_metadata(&metadata_path, &metadata, cancellation).await?;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(offset == 0)
                .append(offset > 0)
                .open(&part)
                .await
                .map_err(map_io)?;
            let expected_response_bytes = response_start
                .map(|range| range.len())
                .unwrap_or(request.descriptor.size);
            let mut response_bytes = 0u64;
            let stream = response.bytes_stream();
            futures_util::pin_mut!(stream);
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(AppError::TaskCancelled),
                    item = stream.next() => match item {
                        Some(Ok(bytes)) => bytes,
                        Some(Err(error)) => return Err(map_request_error(error)),
                        None => break,
                    }
                };
                response_bytes += next.len() as u64;
                if response_bytes > expected_response_bytes {
                    return Err(AppError::ResumeNotSupported);
                }
                file.write_all(&next).await.map_err(map_io)?;
                file.sync_data().await.map_err(map_io)?;
                check_cancelled(cancellation)?;
                hasher.update(&next);
                network_bytes += next.len() as u64;
                metadata.downloaded = offset + response_bytes;
                write_metadata(&metadata_path, &metadata, cancellation).await?;
                event(BlobEvent::Progress(BlobProgress {
                    downloaded: metadata.downloaded,
                    total: request.descriptor.size,
                    network_bytes,
                    network_speed: speed_window.record(Instant::now(), network_bytes),
                }));
            }
            file.flush().await.map_err(map_io)?;
            file.sync_all().await.map_err(map_io)?;
            check_cancelled(cancellation)?;
            if response_bytes != expected_response_bytes {
                return Err(AppError::ResumeNotSupported);
            }
            offset += response_bytes;
            if offset >= request.descriptor.size {
                break;
            }
        }
        event(BlobEvent::Verifying);
        if offset != request.descriptor.size || format!("{:x}", hasher.finalize()) != hex {
            remove_if_exists(&part).await?;
            remove_if_exists(&metadata_path).await?;
            return Err(AppError::DigestMismatch);
        }
        promote(&part, &target, cancellation).await?;
        cleanup_after_commit(&metadata_path).await;
        Ok(target)
    }
}

fn digest_hex(digest: &str) -> Result<String, AppError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(AppError::DigestMismatch);
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::DigestMismatch);
    }
    Ok(hex.to_ascii_lowercase())
}

fn normalize_image(image: &ImageReference) -> ImageReference {
    ImageReference {
        registry: image
            .registry
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase(),
        repository: image.repository.trim_matches('/').to_ascii_lowercase(),
        reference: image.reference.clone(),
    }
}

fn metadata_matches(actual: &ResumeMetadata, expected: &ResumeMetadata) -> bool {
    actual.version == expected.version
        && actual.image == expected.image
        && actual.platform == expected.platform
        && actual.manifest_digest == expected.manifest_digest
        && actual.descriptor == expected.descriptor
}

fn validate_content_range(
    value: Option<&reqwest::header::HeaderValue>,
    offset: u64,
    total: u64,
) -> Result<ContentRange, AppError> {
    let value = value
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::ResumeNotSupported)?;
    let (range, declared_total) = value
        .strip_prefix("bytes ")
        .and_then(|v| v.split_once('/'))
        .ok_or(AppError::ResumeNotSupported)?;
    let (start, end) = range.split_once('-').ok_or(AppError::ResumeNotSupported)?;
    let start = start
        .parse::<u64>()
        .map_err(|_| AppError::ResumeNotSupported)?;
    let end = end
        .parse::<u64>()
        .map_err(|_| AppError::ResumeNotSupported)?;
    let declared_total = declared_total
        .parse::<u64>()
        .map_err(|_| AppError::ResumeNotSupported)?;
    if start != offset || end < start || end >= total || declared_total != total {
        return Err(AppError::ResumeNotSupported);
    }
    Ok(ContentRange { start, end })
}

#[derive(Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
}

impl ContentRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

fn validate_content_length(
    value: Option<&reqwest::header::HeaderValue>,
    expected: u64,
) -> Result<(), AppError> {
    if let Some(actual) = value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        if actual != expected {
            return Err(AppError::ResumeNotSupported);
        }
    }
    Ok(())
}

async fn file_is_verified(
    path: &Path,
    size: u64,
    digest: &str,
    cancellation: &CancellationToken,
) -> Result<bool, AppError> {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(false);
    };
    if metadata.len() != size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    hash_prefix(path, u64::MAX, &mut hasher, cancellation).await?;
    Ok(format!("{:x}", hasher.finalize()) == digest)
}
async fn hash_prefix(
    path: &Path,
    limit: u64,
    hasher: &mut Sha256,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    check_cancelled(cancellation)?;
    let mut file = tokio::fs::File::open(path).await.map_err(map_io)?;
    let mut remaining = limit;
    let mut buf = vec![0; 64 * 1024];
    while remaining > 0 {
        check_cancelled(cancellation)?;
        let chunk_size = buf.len().min(remaining as usize);
        let read = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::TaskCancelled),
            read = file.read(&mut buf[..chunk_size]) => read.map_err(map_io)?,
        };
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        remaining -= read as u64;
    }
    Ok(())
}
async fn read_metadata(path: &Path) -> Option<ResumeMetadata> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}
async fn write_metadata(
    path: &Path,
    metadata: &ResumeMetadata,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    check_cancelled(cancellation)?;
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(metadata).map_err(|_| AppError::ConfigIoFailed)?;
    let mut file = tokio::fs::File::create(&tmp).await.map_err(map_io)?;
    file.write_all(&data).await.map_err(map_io)?;
    file.flush().await.map_err(map_io)?;
    file.sync_all().await.map_err(map_io)?;
    check_cancelled(cancellation)?;
    atomic_replace(&tmp, path).await?;
    check_cancelled(cancellation)
}
async fn promote(
    part: &Path,
    target: &Path,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    check_cancelled(cancellation)?;
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .await
        .map_err(map_io)?;
    file.sync_all().await.map_err(map_io)?;
    check_cancelled(cancellation)?;
    atomic_replace(part, target).await
}

async fn cleanup_after_commit(metadata_path: &Path) {
    let marker = metadata_path.with_extension("cleanup-pending");
    if remove_if_exists(metadata_path).await.is_ok() {
        let _ = remove_if_exists(&marker).await;
    } else if tokio::fs::write(&marker, b"metadata cleanup pending")
        .await
        .is_err()
    {
        eprintln!("blob committed; resume metadata cleanup remains pending");
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), AppError> {
    if cancellation.is_cancelled() {
        Err(AppError::TaskCancelled)
    } else {
        Ok(())
    }
}
async fn remove_if_exists(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(map_io(e)),
    }
}
fn map_status(status: StatusCode) -> AppError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AppError::RegistryAuthFailed,
        StatusCode::NOT_FOUND => AppError::RegistryNotFound,
        _ => AppError::InternalError(format!("blob request failed with HTTP {status}")),
    }
}
fn map_request_error(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::NetworkTimeout
    } else {
        AppError::InternalError("blob network request failed".into())
    }
}
fn map_io(_: std::io::Error) -> AppError {
    AppError::OutputNotWritable
}

#[cfg(not(windows))]
async fn atomic_replace(source: &Path, destination: &Path) -> Result<(), AppError> {
    tokio::fs::rename(source, destination).await.map_err(map_io)
}

#[cfg(windows)]
async fn atomic_replace(source: &Path, destination: &Path) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    tokio::task::spawn_blocking(move || {
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(AppError::OutputNotWritable)
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|_| AppError::OutputNotWritable)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_window_uses_only_the_last_second_byte_delta() {
        let started = Instant::now();
        let mut window = SpeedWindow::new(started);

        assert_eq!(window.record(started, 100), 0.0);
        assert_eq!(
            window.record(started + Duration::from_millis(250), 200),
            400.0
        );
        assert_eq!(
            window.record(started + Duration::from_millis(750), 400),
            400.0
        );
        assert_eq!(
            window.record(started + Duration::from_millis(1_250), 600),
            400.0
        );
        assert_eq!(
            window.record(started + Duration::from_millis(2_250), 650),
            50.0
        );
    }
}
