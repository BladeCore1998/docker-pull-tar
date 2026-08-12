use async_trait::async_trait;
use docker_pull_tar_lib::archive::{ArchiveRequest, ArchiveResult};
use docker_pull_tar_lib::blob::{BlobEvent, BlobProgress, BlobRequest};
use docker_pull_tar_lib::credentials::MemoryCredentialStore;
use docker_pull_tar_lib::domain::{
    AppSettings, Descriptor, DownloadStartRequest, DownloadStatus, ImageReference, Platform,
    ResolvedImage,
};
use docker_pull_tar_lib::download_pipeline::{
    ArchiveService, BlobEventCallback, BlobService, DiskSpaceService, ProductionDownloadPipeline,
    RegistryService, required_download_space, required_packing_space,
};
use docker_pull_tar_lib::error::AppError;
use docker_pull_tar_lib::events::EventSink;
use docker_pull_tar_lib::registry::RegistrySession;
use docker_pull_tar_lib::storage::ConfigStore;
use docker_pull_tar_lib::task_manager::TaskManager;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[test]
fn production_pipeline_exposes_injectable_service_boundaries() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<ProductionDownloadPipeline>();
    let _ = std::any::type_name::<dyn RegistryService>();
    let _ = std::any::type_name::<dyn BlobService>();
    let _ = std::any::type_name::<dyn ArchiveService>();
}

struct FakeRegistry {
    authorization: Option<String>,
}

#[async_trait]
impl RegistryService for FakeRegistry {
    async fn resolve_session(
        &self,
        image: &ImageReference,
        platform: Platform,
        _credentials: Option<(&str, &str)>,
    ) -> Result<RegistrySession, AppError> {
        let mut resolved = resolved_with_layers();
        resolved.registry = image.registry.clone();
        resolved.repository = image.repository.clone();
        resolved.reference = image.reference.clone();
        resolved.platform = platform;
        Ok(RegistrySession::from_parts(
            resolved,
            self.authorization.clone(),
        ))
    }
}

#[derive(Default)]
struct FakeBlobs {
    active: AtomicUsize,
    max_active: AtomicUsize,
    authorizations: Mutex<Vec<Option<String>>>,
    fail_digest: StdMutex<Option<String>>,
    delay_ms: AtomicU64,
}

#[async_trait]
impl BlobService for FakeBlobs {
    async fn download(
        &self,
        root: &Path,
        request: BlobRequest,
        _cancellation: &CancellationToken,
        event: BlobEventCallback,
    ) -> Result<PathBuf, AppError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.authorizations
            .lock()
            .await
            .push(request.authorization.clone());
        event(BlobEvent::Progress(BlobProgress {
            downloaded: request.descriptor.size,
            total: request.descriptor.size,
            network_bytes: request.descriptor.size,
            network_speed: 100.0,
        }));
        event(BlobEvent::Verifying);
        if self.fail_digest.lock().unwrap().as_deref() == Some(&request.descriptor.digest) {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::DigestMismatch);
        }
        tokio::select! {
            _ = _cancellation.cancelled() => {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err(AppError::TaskCancelled);
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(
                self.delay_ms.load(Ordering::SeqCst),
            )) => {}
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(root.join(request.descriptor.digest.replace(':', "_")))
    }
}

#[derive(Default)]
struct FakeArchive {
    request: Mutex<Option<ArchiveRequest>>,
    unpacked_size: AtomicU64,
}

#[async_trait]
impl ArchiveService for FakeArchive {
    async fn estimate_unpacked_size(
        &self,
        _image: &ResolvedImage,
        _layer_paths: &[PathBuf],
        _cancellation: &CancellationToken,
    ) -> Result<u64, AppError> {
        Ok(self.unpacked_size.load(Ordering::SeqCst))
    }

    async fn build(
        &self,
        request: ArchiveRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ArchiveResult, AppError> {
        let result = ArchiveResult {
            path: request
                .output_dir
                .join(format!("{}.tar", request.output_name)),
            size: 60,
        };
        *self.request.lock().await = Some(request);
        Ok(result)
    }
}

#[derive(Default)]
struct RecordingEvents(StdMutex<Vec<(&'static str, Value)>>);

impl EventSink for RecordingEvents {
    fn emit(&self, event: &'static str, payload: Value) {
        self.0.lock().unwrap().push((event, payload));
    }
}

struct FakeDiskSpace(u64);

#[async_trait]
impl DiskSpaceService for FakeDiskSpace {
    async fn available_bytes(&self, _path: &Path) -> Result<u64, AppError> {
        Ok(self.0)
    }
}

#[tokio::test]
async fn pipeline_reuses_bearer_bounds_workers_preserves_layer_order_and_archives() {
    let fixture = Fixture::new(Some("Bearer registry-session".into()), 2).await;
    let snapshot = fixture.run().await;

    assert_eq!(snapshot.status, DownloadStatus::Completed);
    assert_eq!(fixture.blobs.max_active.load(Ordering::SeqCst), 2);
    let authorizations = fixture.blobs.authorizations.lock().await;
    assert_eq!(authorizations.len(), 11);
    assert!(
        authorizations
            .iter()
            .all(|authorization| authorization.as_deref() == Some("Bearer registry-session"))
    );
    let archive = fixture.archive.request.lock().await;
    let archive = archive.as_ref().expect("archive request");
    assert!(
        archive
            .config_path
            .ends_with(format!("sha256_{}", "a".repeat(64)))
    );
    assert!(archive.layer_paths[0].ends_with(format!("sha256_{}", "b".repeat(64))));
    assert!(archive.layer_paths[1].ends_with(format!("sha256_{}", "c".repeat(64))));

    let events = fixture.events.0.lock().unwrap();
    let progress_events: Vec<_> = events
        .iter()
        .filter(|(event, _)| *event == "download://progress")
        .map(|(_, payload)| payload)
        .collect();
    assert!(
        progress_events[0]["layers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|layer| layer["status"] == "waiting")
    );
    let first_layer_statuses: Vec<_> = progress_events
        .iter()
        .map(|payload| payload["layers"][0]["status"].as_str().unwrap())
        .collect();
    for status in ["downloading", "verifying", "completed"] {
        assert!(first_layer_statuses.contains(&status), "missing {status:?}");
    }
    assert!(progress_events.iter().any(|payload| {
        payload["speed"]
            .as_f64()
            .is_some_and(|speed| speed >= 100.0)
    }));
    let logs = events
        .iter()
        .filter(|(event, _)| *event == "download://log")
        .map(|(_, payload)| payload["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        logs,
        [
            "Resolving image manifest",
            "Downloading image blobs",
            "Verifying downloaded image",
            "Packing Docker archive",
        ]
    );
    assert!(
        !serde_json::to_string(&*events)
            .unwrap()
            .contains("password")
    );
}

#[tokio::test]
async fn anonymous_registry_session_keeps_blob_authorization_empty() {
    let fixture = Fixture::new(None, 1).await;
    fixture.run().await;

    let authorizations = fixture.blobs.authorizations.lock().await;
    assert_eq!(authorizations.len(), 11);
    assert!(
        authorizations
            .iter()
            .all(|authorization| authorization.is_none())
    );
    assert_eq!(fixture.blobs.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execution_clamps_legacy_worker_setting_to_eight_without_rewriting_it() {
    let fixture = Fixture::new(Some("Bearer session".into()), 99).await;
    fixture.run().await;

    assert_eq!(fixture.blobs.max_active.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn disk_preflight_uses_saturating_exact_budget_and_skips_archive_when_short() {
    let required = required_download_space(&resolved_with_layers());
    assert_eq!(required_download_space(&overflowing_resolved()), u64::MAX);
    let fixture = Fixture::with_registry_and_disk(
        Arc::new(FakeRegistry {
            authorization: None,
        }),
        4,
        required - 1,
    )
    .await;
    let snapshot = fixture.run().await;

    assert_eq!(snapshot.status, DownloadStatus::Failed);
    assert!(
        snapshot
            .last_error
            .as_deref()
            .unwrap()
            .contains("insufficient disk space")
    );
    assert!(fixture.archive.request.lock().await.is_none());
    assert!(
        snapshot
            .progress
            .layers
            .iter()
            .all(|layer| layer.status == docker_pull_tar_lib::domain::LayerStatus::Failed)
    );
}

#[tokio::test]
async fn packing_preflight_rejects_actual_layer_expansion_above_three_times_compressed_size() {
    let image = resolved_with_layers();
    let compressed_layers = image.layers.iter().map(|layer| layer.size).sum::<u64>();
    let actual_unpacked = compressed_layers * 4;
    let required = required_packing_space(&image, actual_unpacked);
    assert!(required > required_download_space(&image));
    let fixture = Fixture::with_registry_and_disk(
        Arc::new(FakeRegistry {
            authorization: None,
        }),
        4,
        required - 1,
    )
    .await;
    fixture
        .archive
        .unpacked_size
        .store(actual_unpacked, Ordering::SeqCst);

    let snapshot = fixture.run().await;

    assert_eq!(snapshot.status, DownloadStatus::Failed);
    assert!(
        snapshot
            .last_error
            .as_deref()
            .unwrap()
            .contains("insufficient disk space")
    );
    assert!(fixture.archive.request.lock().await.is_none());
}

#[tokio::test]
async fn cancellation_marks_unfinished_layers_and_emits_terminal_progress() {
    let fixture = Fixture::new(None, 2).await;
    fixture.blobs.delay_ms.store(500, Ordering::SeqCst);
    fixture.manager.start(request(&fixture._dir)).await.unwrap();
    for _ in 0..100 {
        if fixture.blobs.active.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(fixture.manager.cancel().await);
    let snapshot = fixture.wait_for_terminal().await;
    assert_eq!(snapshot.status, DownloadStatus::Cancelled);
    assert!(snapshot.progress.layers.iter().all(|layer| matches!(
        layer.status,
        docker_pull_tar_lib::domain::LayerStatus::Completed
            | docker_pull_tar_lib::domain::LayerStatus::Cancelled
    )));
    assert!(
        snapshot
            .progress
            .layers
            .iter()
            .any(|layer| { layer.status == docker_pull_tar_lib::domain::LayerStatus::Cancelled })
    );
}

#[tokio::test]
async fn verifying_is_observable_before_blob_future_returns() {
    let fixture = Fixture::new(None, 1).await;
    fixture.blobs.delay_ms.store(500, Ordering::SeqCst);
    fixture.manager.start(request(&fixture._dir)).await.unwrap();

    let observed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let has_verifying = fixture
                .events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(event, payload)| {
                    *event == "download://progress"
                        && payload["layers"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|layer| layer["status"] == "verifying")
                });
            if has_verifying {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(observed.is_ok());
    assert_eq!(
        fixture.manager.snapshot().await.status,
        DownloadStatus::Downloading
    );
    assert!(fixture.archive.request.lock().await.is_none());
    fixture.manager.cancel().await;
    fixture.wait_for_terminal().await;
}

#[tokio::test]
async fn blob_failure_marks_unfinished_layers_failed_and_skips_archive() {
    let fixture = Fixture::new(None, 2).await;
    *fixture.blobs.fail_digest.lock().unwrap() = Some(digest('b'));
    let snapshot = fixture.run().await;

    assert_eq!(snapshot.status, DownloadStatus::Failed);
    assert!(
        snapshot
            .progress
            .layers
            .iter()
            .any(|layer| { layer.status == docker_pull_tar_lib::domain::LayerStatus::Failed })
    );
    assert!(snapshot.progress.layers.iter().all(|layer| matches!(
        layer.status,
        docker_pull_tar_lib::domain::LayerStatus::Completed
            | docker_pull_tar_lib::domain::LayerStatus::Failed
    )));
    assert!(fixture.archive.request.lock().await.is_none());
    let events = fixture.events.0.lock().unwrap();
    assert!(events.iter().rev().any(|(event, payload)| {
        *event == "download://progress"
            && payload["layers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|layer| layer["status"] == "failed")
    }));
}

struct HangingRegistry {
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RegistryService for HangingRegistry {
    async fn resolve_session(
        &self,
        _image: &ImageReference,
        _platform: Platform,
        _credentials: Option<(&str, &str)>,
    ) -> Result<RegistrySession, AppError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_registry_resolution_during_preparing() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let fixture = Fixture::with_registry(
        Arc::new(HangingRegistry {
            entered: entered.clone(),
        }),
        4,
    )
    .await;
    fixture.manager.start(request(&fixture._dir)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    assert!(fixture.manager.cancel().await);

    let snapshot = fixture.wait_for_terminal().await;
    assert_eq!(snapshot.status, DownloadStatus::Cancelled);
}

#[test]
fn registry_session_debug_redacts_authorization() {
    let session = RegistrySession::from_parts(resolved(), Some("Bearer secret-value".into()));
    let debug = format!("{session:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("secret-value"));
}

struct Fixture {
    _dir: TempDir,
    manager: TaskManager,
    blobs: Arc<FakeBlobs>,
    archive: Arc<FakeArchive>,
    events: Arc<RecordingEvents>,
}

impl Fixture {
    async fn new(authorization: Option<String>, workers: u32) -> Self {
        Self::with_registry(Arc::new(FakeRegistry { authorization }), workers).await
    }

    async fn with_registry(registry: Arc<dyn RegistryService>, workers: u32) -> Self {
        Self::with_registry_and_disk(registry, workers, u64::MAX).await
    }

    async fn with_registry_and_disk(
        registry: Arc<dyn RegistryService>,
        workers: u32,
        available: u64,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigStore::open(
            dir.path().join("config.json"),
            AppSettings {
                default_output_dir: dir.path().to_string_lossy().into_owned(),
                default_arch: "amd64".into(),
                default_mirror: "registry.test".into(),
                download_workers: workers,
                last_package_name: None,
            },
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        let blobs = Arc::new(FakeBlobs {
            delay_ms: AtomicU64::new(20),
            ..FakeBlobs::default()
        });
        let archive = Arc::new(FakeArchive::default());
        let events = Arc::new(RecordingEvents::default());
        let pipeline = Arc::new(ProductionDownloadPipeline::new(
            registry,
            blobs.clone(),
            archive.clone(),
            Arc::new(FakeDiskSpace(available)),
            config.clone(),
        ));
        let manager = TaskManager::new(pipeline, events.clone(), Arc::new(config));
        Self {
            _dir: dir,
            manager,
            blobs,
            archive,
            events,
        }
    }

    async fn run(&self) -> docker_pull_tar_lib::domain::DownloadSnapshot {
        self.manager.start(request(&self._dir)).await.unwrap();
        self.wait_for_terminal().await
    }

    async fn wait_for_terminal(&self) -> docker_pull_tar_lib::domain::DownloadSnapshot {
        for _ in 0..100 {
            let snapshot = self.manager.snapshot().await;
            if matches!(
                snapshot.status,
                DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Cancelled
            ) {
                return snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("pipeline did not reach a terminal state")
    }
}

fn request(dir: &TempDir) -> DownloadStartRequest {
    DownloadStartRequest {
        package_name: "team/app".into(),
        tag: "latest".into(),
        arch: "amd64".into(),
        mirror: "registry.test".into(),
        output_dir: dir.path().to_string_lossy().into_owned(),
        username: Some("alice".into()),
        password: Some("password".into()),
    }
}

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn descriptor(character: char, size: u64, media_type: &str) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: digest(character),
        size,
    }
}

fn resolved() -> ResolvedImage {
    ResolvedImage {
        registry: "registry.test".into(),
        repository: "team/app".into(),
        reference: "latest".into(),
        manifest_digest: digest('f'),
        config: descriptor('a', 10, "application/vnd.oci.image.config.v1+json"),
        layers: vec![],
        platform: Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
    }
}

fn resolved_with_layers() -> ResolvedImage {
    ResolvedImage {
        layers: ('b'..='k')
            .enumerate()
            .map(|(index, character)| {
                descriptor(
                    character,
                    20 + index as u64,
                    "application/vnd.oci.image.layer.v1.tar",
                )
            })
            .collect(),
        ..resolved()
    }
}

fn overflowing_resolved() -> ResolvedImage {
    ResolvedImage {
        config: descriptor('a', u64::MAX, "application/vnd.oci.image.config.v1+json"),
        layers: vec![descriptor(
            'b',
            u64::MAX,
            "application/vnd.oci.image.layer.v1.tar",
        )],
        ..resolved()
    }
}
