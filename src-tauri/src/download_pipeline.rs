use crate::archive::{ArchiveRequest, ArchiveResult, build_archive, estimate_unpacked_size};
use crate::blob::{BlobDownloader, BlobEvent, BlobRequest};
use crate::domain::{
    CompletedDownload, Descriptor, DownloadProgress, DownloadSnapshot, DownloadStartRequest,
    DownloadStatus, ImageReference, LayerProgress, LayerStatus, Platform,
};
use crate::error::AppError;
use crate::image_ref::parse_image_reference;
use crate::registry::{RegistryClient, RegistrySession, blob_url};
use crate::storage::ConfigStore;
use crate::task_manager::{DownloadPipeline, TaskContext, TaskManager};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type BlobEventCallback = Arc<dyn Fn(BlobEvent) + Send + Sync>;

#[async_trait]
pub trait RegistryService: Send + Sync {
    async fn resolve_session(
        &self,
        image: &ImageReference,
        platform: Platform,
        credentials: Option<(&str, &str)>,
    ) -> Result<RegistrySession, AppError>;
}

#[async_trait]
impl RegistryService for RegistryClient {
    async fn resolve_session(
        &self,
        image: &ImageReference,
        platform: Platform,
        credentials: Option<(&str, &str)>,
    ) -> Result<RegistrySession, AppError> {
        RegistryClient::resolve_session(self, image, platform, credentials).await
    }
}

#[async_trait]
pub trait BlobService: Send + Sync {
    async fn download(
        &self,
        root: &Path,
        request: BlobRequest,
        cancellation: &CancellationToken,
        event: BlobEventCallback,
    ) -> Result<PathBuf, AppError>;
}

pub struct FileBlobService;

#[async_trait]
impl BlobService for FileBlobService {
    async fn download(
        &self,
        root: &Path,
        request: BlobRequest,
        cancellation: &CancellationToken,
        event: BlobEventCallback,
    ) -> Result<PathBuf, AppError> {
        BlobDownloader::new(root)?
            .download_with_events(request, cancellation, move |update| event(update))
            .await
    }
}

#[async_trait]
pub trait ArchiveService: Send + Sync {
    async fn estimate_unpacked_size(
        &self,
        image: &crate::domain::ResolvedImage,
        layer_paths: &[PathBuf],
        cancellation: &CancellationToken,
    ) -> Result<u64, AppError>;

    async fn build(
        &self,
        request: ArchiveRequest,
        cancellation: &CancellationToken,
    ) -> Result<ArchiveResult, AppError>;
}

pub struct DockerArchiveService;

#[async_trait]
impl ArchiveService for DockerArchiveService {
    async fn estimate_unpacked_size(
        &self,
        image: &crate::domain::ResolvedImage,
        layer_paths: &[PathBuf],
        cancellation: &CancellationToken,
    ) -> Result<u64, AppError> {
        estimate_unpacked_size(image, layer_paths, cancellation).await
    }

    async fn build(
        &self,
        request: ArchiveRequest,
        cancellation: &CancellationToken,
    ) -> Result<ArchiveResult, AppError> {
        build_archive(request, cancellation).await
    }
}

#[async_trait]
pub trait DiskSpaceService: Send + Sync {
    async fn available_bytes(&self, path: &Path) -> Result<u64, AppError>;
}

pub struct SystemDiskSpaceService;

#[async_trait]
impl DiskSpaceService for SystemDiskSpaceService {
    async fn available_bytes(&self, path: &Path) -> Result<u64, AppError> {
        available_disk_space(path)
    }
}

#[cfg(windows)]
fn available_disk_space(path: &Path) -> Result<u64, AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let existing = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .ok_or(AppError::OutputNotWritable)?;
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0_u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(AppError::OutputNotWritable)
    } else {
        Ok(available)
    }
}

#[cfg(not(windows))]
fn available_disk_space(_path: &Path) -> Result<u64, AppError> {
    Ok(u64::MAX)
}

const METADATA_BASE_RESERVE: u64 = 16 * 1024 * 1024;
const METADATA_PER_LAYER_RESERVE: u64 = 64 * 1024;

fn metadata_reserve(layer_count: usize) -> u64 {
    METADATA_BASE_RESERVE
        .saturating_add((layer_count as u64).saturating_mul(METADATA_PER_LAYER_RESERVE))
}

pub fn required_download_space(image: &crate::domain::ResolvedImage) -> u64 {
    let layer_blobs = image
        .layers
        .iter()
        .fold(0_u64, |sum, layer| sum.saturating_add(layer.size));
    let remaining_blobs = image.config.size.saturating_add(layer_blobs);
    remaining_blobs.saturating_add(metadata_reserve(image.layers.len()))
}

pub fn required_packing_space(image: &crate::domain::ResolvedImage, unpacked_layers: u64) -> u64 {
    let final_archive = image.config.size.saturating_add(unpacked_layers);
    unpacked_layers
        .saturating_add(final_archive)
        .saturating_add(metadata_reserve(image.layers.len()))
}

pub struct ProductionDownloadPipeline {
    registry: Arc<dyn RegistryService>,
    blobs: Arc<dyn BlobService>,
    archive: Arc<dyn ArchiveService>,
    disk_space: Arc<dyn DiskSpaceService>,
    config: ConfigStore,
}

impl ProductionDownloadPipeline {
    pub fn new(
        registry: Arc<dyn RegistryService>,
        blobs: Arc<dyn BlobService>,
        archive: Arc<dyn ArchiveService>,
        disk_space: Arc<dyn DiskSpaceService>,
        config: ConfigStore,
    ) -> Self {
        Self {
            registry,
            blobs,
            archive,
            disk_space,
            config,
        }
    }

    pub fn from_config(config: ConfigStore) -> Result<Self, AppError> {
        Ok(Self::new(
            Arc::new(RegistryClient::new()?),
            Arc::new(FileBlobService),
            Arc::new(DockerArchiveService),
            Arc::new(SystemDiskSpaceService),
            config,
        ))
    }
}

#[async_trait]
pub trait DownloadService: Send + Sync {
    async fn start(&self, request: DownloadStartRequest) -> Result<DownloadSnapshot, AppError>;
    async fn cancel(&self) -> DownloadSnapshot;
    async fn status(&self) -> DownloadSnapshot;
}

#[derive(Clone)]
pub struct DownloadCoordinator {
    config: ConfigStore,
    tasks: TaskManager,
}

impl DownloadCoordinator {
    pub fn new(config: ConfigStore, tasks: TaskManager) -> Self {
        Self { config, tasks }
    }
}

#[async_trait]
impl DownloadService for DownloadCoordinator {
    async fn start(&self, mut request: DownloadStartRequest) -> Result<DownloadSnapshot, AppError> {
        if let Some(mirror) = self
            .config
            .mirrors()
            .await?
            .into_iter()
            .find(|mirror| mirror.id == request.mirror || mirror.registry == request.mirror)
        {
            request.mirror = mirror.registry;
            request.username = request.username.or(mirror.username);
            request.password = request.password.or(mirror.password);
        }
        self.tasks.start(request).await?;
        Ok(self.tasks.snapshot().await)
    }

    async fn cancel(&self) -> DownloadSnapshot {
        self.tasks.cancel().await;
        self.tasks.snapshot().await
    }

    async fn status(&self) -> DownloadSnapshot {
        self.tasks.snapshot().await
    }
}

#[derive(Clone, Copy)]
enum JobKind {
    Config,
    Layer(usize),
}

struct DownloadJob {
    kind: JobKind,
    descriptor: Descriptor,
}

struct JobUpdate {
    kind: JobKind,
    event: BlobEvent,
}

impl JobKind {
    fn speed_index(self) -> usize {
        match self {
            Self::Config => 0,
            Self::Layer(index) => index + 1,
        }
    }
}

#[async_trait]
impl DownloadPipeline for ProductionDownloadPipeline {
    async fn run(
        &self,
        request: DownloadStartRequest,
        context: TaskContext,
    ) -> Result<CompletedDownload, AppError> {
        let image = request_image(&request)?;
        let platform = requested_platform(&request.arch)?;
        let credentials = request.username.as_deref().zip(request.password.as_deref());
        let cancellation = context.cancellation_token();
        if !context.log("info", "Resolving image manifest").await {
            return Err(AppError::TaskCancelled);
        }
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::TaskCancelled),
            session = self.registry.resolve_session(&image, platform, credentials) => session?,
        };
        transition(&context, DownloadStatus::Downloading).await?;
        if !context.log("info", "Downloading image blobs").await {
            return Err(AppError::TaskCancelled);
        }

        let output_dir = PathBuf::from(&request.output_dir);
        let workers = self.config.settings().await.download_workers.clamp(1, 8) as usize;
        let mut progress = initial_progress(&session.image.config, &session.image.layers);
        if !context.report_progress(progress.clone(), false).await {
            mark_incomplete_layers(&mut progress, LayerStatus::Cancelled);
            context.report_terminal_progress(progress).await;
            return Err(AppError::TaskCancelled);
        }
        let required = required_download_space(&session.image);
        let available = match self.disk_space.available_bytes(&output_dir).await {
            Ok(available) => available,
            Err(error) => {
                mark_incomplete_layers(&mut progress, LayerStatus::Failed);
                context.report_terminal_progress(progress).await;
                return Err(error);
            }
        };
        if available < required {
            mark_incomplete_layers(&mut progress, LayerStatus::Failed);
            context.report_terminal_progress(progress).await;
            return Err(AppError::DiskSpaceInsufficient);
        }
        let mut jobs = Vec::with_capacity(session.image.layers.len() + 1);
        jobs.push(DownloadJob {
            kind: JobKind::Config,
            descriptor: session.image.config.clone(),
        });
        jobs.extend(
            session
                .image
                .layers
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, descriptor)| DownloadJob {
                    kind: JobKind::Layer(index),
                    descriptor,
                }),
        );

        let mut config_downloaded = 0;
        let mut speeds = vec![0.0; session.image.layers.len() + 1];
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel::<JobUpdate>();
        let authorization = session.blob_authorization().map(str::to_owned);
        let blobs = self.blobs.clone();
        let download_root = output_dir.clone();
        let download_image = image.clone();
        let download_platform = session.image.platform.clone();
        let manifest_digest = session.image.manifest_digest.clone();
        let download_cancellation = cancellation.clone();
        let job_updates = updates_tx.clone();
        let downloads = stream::iter(jobs.into_iter().map(move |job| {
            let blobs = blobs.clone();
            let root = download_root.clone();
            let image = download_image.clone();
            let platform = download_platform.clone();
            let manifest_digest = manifest_digest.clone();
            let authorization = authorization.clone();
            let cancellation = download_cancellation.clone();
            let updates = job_updates.clone();
            async move {
                let kind = job.kind;
                let callback: BlobEventCallback = Arc::new(move |event| {
                    let _ = updates.send(JobUpdate { kind, event });
                });
                let result = async {
                    let url = blob_url(&image, &job.descriptor.digest)?;
                    blobs
                        .download(
                            &root,
                            BlobRequest {
                                url,
                                image,
                                platform,
                                manifest_digest,
                                descriptor: job.descriptor,
                                authorization,
                            },
                            &cancellation,
                            callback,
                        )
                        .await
                }
                .await;
                (kind, result)
            }
        }))
        .buffer_unordered(workers);
        drop(updates_tx);
        tokio::pin!(downloads);

        let mut config_path = None;
        let mut layer_paths = vec![None; session.image.layers.len()];
        let mut downloads_finished = false;
        while !downloads_finished {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    mark_incomplete_layers(&mut progress, LayerStatus::Cancelled);
                    context.report_terminal_progress(progress).await;
                    return Err(AppError::TaskCancelled);
                },
                Some(update) = updates_rx.recv() => {
                    let force_event = apply_blob_event(
                        &mut progress, &mut config_downloaded, &mut speeds, update,
                    );
                    if !context.report_progress(progress.clone(), force_event).await {
                        mark_incomplete_layers(&mut progress, LayerStatus::Cancelled);
                        context.report_terminal_progress(progress).await;
                        return Err(AppError::TaskCancelled);
                    }
                }
                result = downloads.next() => match result {
                    Some((JobKind::Config, Ok(path))) => {
                        speeds[0] = 0.0;
                        config_path = Some(path);
                        config_downloaded = session.image.config.size;
                        recompute_totals(&mut progress, config_downloaded, &speeds);
                    },
                    Some((JobKind::Layer(index), Ok(path))) => {
                        speeds[index + 1] = 0.0;
                        layer_paths[index] = Some(path);
                        progress.layers[index].status = LayerStatus::Completed;
                        progress.layers[index].downloaded = progress.layers[index].total;
                        progress.current_layer = layer_paths.iter().filter(|path| path.is_some()).count() as u32;
                        recompute_totals(&mut progress, config_downloaded, &speeds);
                        if !context.report_progress(progress.clone(), true).await {
                            mark_incomplete_layers(&mut progress, LayerStatus::Cancelled);
                            context.report_terminal_progress(progress).await;
                            return Err(AppError::TaskCancelled);
                        }
                    }
                    Some((_kind, Err(error))) => {
                        mark_incomplete_layers(&mut progress, LayerStatus::Failed);
                        context.report_terminal_progress(progress).await;
                        return Err(error);
                    },
                    None => downloads_finished = true,
                }
            }
        }

        if !context.log("info", "Verifying downloaded image").await {
            return Err(AppError::TaskCancelled);
        }
        transition(&context, DownloadStatus::Verifying).await?;
        if !context.log("info", "Packing Docker archive").await {
            return Err(AppError::TaskCancelled);
        }
        transition(&context, DownloadStatus::Packing).await?;
        let config_path = config_path.ok_or_else(|| {
            AppError::InternalError("config blob download completed without a path".into())
        })?;
        let layer_paths = layer_paths
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                AppError::InternalError("layer download completed without a path".into())
            })?;
        let unpacked_layers = self
            .archive
            .estimate_unpacked_size(&session.image, &layer_paths, &cancellation)
            .await?;
        let required = required_packing_space(&session.image, unpacked_layers);
        let available = self.disk_space.available_bytes(&output_dir).await?;
        if available < required {
            return Err(AppError::DiskSpaceInsufficient);
        }
        let output_name = archive_name(&request.package_name, &request.tag);
        let archived = self
            .archive
            .build(
                ArchiveRequest {
                    image: session.image.clone(),
                    config_path,
                    layer_paths,
                    output_dir: output_dir.clone(),
                    output_name,
                },
                &cancellation,
            )
            .await?;
        Ok(CompletedDownload {
            output_path: archived.path.to_string_lossy().into_owned(),
            size_bytes: archived.size,
        })
    }
}

async fn transition(context: &TaskContext, status: DownloadStatus) -> Result<(), AppError> {
    context
        .transition(status)
        .await?
        .then_some(())
        .ok_or(AppError::TaskCancelled)
}

fn request_image(request: &DownloadStartRequest) -> Result<ImageReference, AppError> {
    let separator = if request.tag.starts_with("sha256:") {
        '@'
    } else {
        ':'
    };
    parse_image_reference(
        &format!(
            "{}/{}{}{}",
            request.mirror, request.package_name, separator, request.tag
        ),
        &request.mirror,
    )
}

fn requested_platform(arch: &str) -> Result<Platform, AppError> {
    let (architecture, variant) = match arch.trim().to_ascii_lowercase().as_str() {
        "amd64" | "x86_64" => ("amd64", None),
        "arm64" | "aarch64" => ("arm64", None),
        "arm/v7" | "armv7" => ("arm", Some("v7")),
        _ => return Err(AppError::PlatformNotFound),
    };
    Ok(Platform {
        os: "linux".into(),
        architecture: architecture.into(),
        variant: variant.map(str::to_owned),
    })
}

fn initial_progress(config: &Descriptor, layers: &[Descriptor]) -> DownloadProgress {
    DownloadProgress {
        current_layer: 0,
        total_layers: layers.len() as u32,
        downloaded_bytes: 0,
        total_bytes: config.size.saturating_add(
            layers
                .iter()
                .fold(0_u64, |total, layer| total.saturating_add(layer.size)),
        ),
        speed: 0.0,
        layers: layers
            .iter()
            .map(|layer| LayerProgress {
                digest: layer.digest.clone(),
                size: layer.size,
                status: LayerStatus::Waiting,
                downloaded: 0,
                total: layer.size,
            })
            .collect(),
    }
}

fn apply_blob_event(
    progress: &mut DownloadProgress,
    config_downloaded: &mut u64,
    speeds: &mut [f64],
    update: JobUpdate,
) -> bool {
    let force_event = match update.event {
        BlobEvent::Progress(update_progress) => {
            speeds[update.kind.speed_index()] = update_progress.network_speed;
            match update.kind {
                JobKind::Config => *config_downloaded = update_progress.downloaded,
                JobKind::Layer(index) => {
                    let layer = &mut progress.layers[index];
                    let started = layer.status == LayerStatus::Waiting;
                    layer.status = LayerStatus::Downloading;
                    layer.downloaded = update_progress.downloaded.min(layer.total);
                    if started {
                        recompute_totals(progress, *config_downloaded, speeds);
                        return true;
                    }
                }
            }
            false
        }
        BlobEvent::Verifying => {
            speeds[update.kind.speed_index()] = 0.0;
            if let JobKind::Layer(index) = update.kind {
                progress.layers[index].status = LayerStatus::Verifying;
                true
            } else {
                false
            }
        }
    };
    recompute_totals(progress, *config_downloaded, speeds);
    force_event
}

fn recompute_totals(progress: &mut DownloadProgress, config_downloaded: u64, speeds: &[f64]) {
    progress.downloaded_bytes = progress
        .layers
        .iter()
        .fold(config_downloaded, |total, layer| {
            total.saturating_add(layer.downloaded)
        });
    progress.speed = speeds.iter().copied().sum();
}

fn mark_incomplete_layers(progress: &mut DownloadProgress, status: LayerStatus) {
    for layer in &mut progress.layers {
        if layer.status != LayerStatus::Completed {
            layer.status = status;
        }
    }
    progress.speed = 0.0;
}

fn archive_name(package_name: &str, tag: &str) -> String {
    let raw = format!("{package_name}_{tag}");
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}
