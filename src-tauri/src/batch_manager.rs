use crate::domain::{
    BatchSnapshot, BatchStartRequest, BatchStatus, BatchTaskSnapshot, BatchTaskStatus,
    DownloadStartRequest, DownloadStatus, ImageReference, normalize_registry,
};
use crate::error::AppError;
use crate::events::{BATCH_STATE_EVENT, EventSink};
use crate::events::{DOWNLOAD_LOG_EVENT, DOWNLOAD_PROGRESS_EVENT, DOWNLOAD_STATE_EVENT};
use crate::image_ref::parse_image_reference;
use crate::storage::{ConfigStore, HistoryWriter};
use crate::task_manager::{DownloadPipeline, TaskManager};
use futures_util::future::join_all;
use serde_json::{Value, json, to_value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_BATCH_IMAGES: usize = 10;
const MAX_IMAGE_CONCURRENCY: u32 = 8;

struct BatchTaskRuntime {
    task_id: String,
    request: DownloadStartRequest,
    image: ImageReference,
    manager: TaskManager,
    status: Mutex<BatchTaskStatus>,
    cancel_requested: AtomicBool,
}

struct BatchRuntime {
    snapshot: Mutex<BatchSnapshot>,
    cancellation: CancellationToken,
    tasks: Vec<Arc<BatchTaskRuntime>>,
    semaphore: Arc<Semaphore>,
}

struct BatchManagerInner {
    config: ConfigStore,
    pipeline: Arc<dyn DownloadPipeline>,
    events: Arc<dyn EventSink>,
    history: Arc<dyn HistoryWriter>,
    batches: Mutex<HashMap<String, Arc<BatchRuntime>>>,
    active_batch: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct BatchManager {
    inner: Arc<BatchManagerInner>,
}

impl BatchManager {
    pub fn new(
        config: ConfigStore,
        pipeline: Arc<dyn DownloadPipeline>,
        events: Arc<dyn EventSink>,
        history: Arc<dyn HistoryWriter>,
    ) -> Self {
        Self {
            inner: Arc::new(BatchManagerInner {
                config,
                pipeline,
                events,
                history,
                batches: Mutex::new(HashMap::new()),
                active_batch: Mutex::new(None),
            }),
        }
    }

    pub async fn start(&self, request: BatchStartRequest) -> Result<BatchSnapshot, AppError> {
        let max_concurrent_images = request.max_concurrent_images;
        if request.images.is_empty() || request.images.len() > MAX_BATCH_IMAGES {
            return Err(AppError::InvalidImageReference(format!(
                "a batch must contain between 1 and {MAX_BATCH_IMAGES} images"
            )));
        }
        if !(1..=MAX_IMAGE_CONCURRENCY).contains(&max_concurrent_images) {
            return Err(AppError::InternalError(format!(
                "max_concurrent_images must be between 1 and {MAX_IMAGE_CONCURRENCY}"
            )));
        }
        let mut active_batch = self.inner.active_batch.lock().await;
        if active_batch.is_some() {
            return Err(AppError::TaskAlreadyRunning);
        }

        let requests = self.resolve_requests(request.images).await?;
        let mut seen = HashSet::new();
        let mut tasks = Vec::with_capacity(requests.len());
        for request in requests {
            let image = request_image(&request)?;
            let key = format!(
                "{}|{}|{}|{}",
                image.registry, image.repository, image.reference, request.arch
            );
            if !seen.insert(key) {
                continue;
            }
            let task_id = Uuid::new_v4().to_string();
            let manager = TaskManager::new(
                self.inner.pipeline.clone(),
                Arc::new(BatchTaskEventSink {
                    inner: self.inner.events.clone(),
                    task_id: task_id.clone(),
                }),
                self.inner.history.clone(),
            );
            tasks.push(Arc::new(BatchTaskRuntime {
                task_id,
                request,
                image,
                manager,
                status: Mutex::new(BatchTaskStatus::Queued),
                cancel_requested: AtomicBool::new(false),
            }));
        }
        if tasks.is_empty() {
            return Err(AppError::InvalidImageReference(
                "the batch contains no unique images".into(),
            ));
        }

        let batch_id = Uuid::new_v4().to_string();
        let snapshot = BatchSnapshot {
            batch_id: batch_id.clone(),
            status: BatchStatus::Queued,
            max_concurrent_images,
            tasks: join_all(tasks.iter().map(task_snapshot)).await,
        };
        let runtime = Arc::new(BatchRuntime {
            snapshot: Mutex::new(snapshot.clone()),
            cancellation: CancellationToken::new(),
            tasks,
            semaphore: Arc::new(Semaphore::new(max_concurrent_images as usize)),
        });
        self.inner
            .batches
            .lock()
            .await
            .insert(batch_id.clone(), runtime.clone());
        *active_batch = Some(batch_id.clone());
        drop(active_batch);
        self.emit_snapshot(&snapshot);

        for task in &runtime.tasks {
            let manager = self.clone();
            let runtime = runtime.clone();
            let task = task.clone();
            let semaphore = runtime.semaphore.clone();
            let batch_id = batch_id.clone();
            tokio::spawn(async move {
                manager.run_task(batch_id, runtime, task, semaphore).await;
            });
        }
        Ok(snapshot)
    }

    async fn resolve_requests(
        &self,
        mut requests: Vec<DownloadStartRequest>,
    ) -> Result<Vec<DownloadStartRequest>, AppError> {
        let mirrors = self.inner.config.mirrors().await?;
        let mut selected_registry = None;
        let mut selected_credentials: Option<(Option<String>, Option<String>)> = None;
        for request in &mut requests {
            if let Some(mirror) = mirrors
                .iter()
                .find(|mirror| mirror.id == request.mirror || mirror.registry == request.mirror)
            {
                request.mirror = mirror.registry.clone();
                request.username = request.username.clone().or(mirror.username.clone());
                request.password = request.password.clone().or(mirror.password.clone());
            }
            let registry = normalize_registry(&request.mirror);
            if registry.is_empty() {
                return Err(AppError::InvalidMirror);
            }
            request.mirror = registry.clone();
            if let Some(credentials) = &selected_credentials {
                if credentials != &(request.username.clone(), request.password.clone()) {
                    return Err(AppError::InvalidMirror);
                }
            } else {
                selected_credentials = Some((request.username.clone(), request.password.clone()));
            }
            if let Some(selected) = &selected_registry {
                if selected != &registry {
                    return Err(AppError::InvalidMirror);
                }
            } else {
                selected_registry = Some(registry);
            }
        }
        Ok(requests)
    }

    async fn run_task(
        &self,
        batch_id: String,
        runtime: Arc<BatchRuntime>,
        task: Arc<BatchTaskRuntime>,
        semaphore: Arc<Semaphore>,
    ) {
        let permit = match semaphore.acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        if runtime.cancellation.is_cancelled() || task.cancel_requested.load(Ordering::Acquire) {
            *task.status.lock().await = BatchTaskStatus::Cancelled;
            drop(permit);
            self.refresh_batch(&batch_id, &runtime).await;
            return;
        }
        *task.status.lock().await = BatchTaskStatus::Running;
        self.refresh_batch(&batch_id, &runtime).await;
        let result = task.manager.start(task.request.clone()).await;
        if result.is_ok() {
            loop {
                let snapshot = task.manager.snapshot().await;
                if matches!(
                    snapshot.status,
                    DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Cancelled
                ) {
                    *task.status.lock().await = match snapshot.status {
                        DownloadStatus::Completed => BatchTaskStatus::Completed,
                        DownloadStatus::Cancelled => BatchTaskStatus::Cancelled,
                        _ => BatchTaskStatus::Failed,
                    };
                    break;
                }
                if runtime.cancellation.is_cancelled() {
                    task.manager.cancel().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } else {
            *task.status.lock().await = if runtime.cancellation.is_cancelled() {
                BatchTaskStatus::Cancelled
            } else {
                BatchTaskStatus::Failed
            };
        }
        drop(permit);
        self.refresh_batch(&batch_id, &runtime).await;
    }

    pub async fn status(&self, batch_id: &str) -> Result<BatchSnapshot, AppError> {
        let runtime = self
            .inner
            .batches
            .lock()
            .await
            .get(batch_id)
            .cloned()
            .ok_or(AppError::InvalidImageReference(batch_id.into()))?;
        self.refresh_batch(batch_id, &runtime).await;
        let snapshot = runtime.snapshot.lock().await.clone();
        Ok(snapshot)
    }

    pub async fn cancel(&self, batch_id: &str) -> Result<BatchSnapshot, AppError> {
        let runtime = self
            .inner
            .batches
            .lock()
            .await
            .get(batch_id)
            .cloned()
            .ok_or(AppError::InvalidImageReference(batch_id.into()))?;
        runtime.cancellation.cancel();
        runtime.snapshot.lock().await.status = BatchStatus::Cancelling;
        for task in &runtime.tasks {
            let status = *task.status.lock().await;
            match status {
                BatchTaskStatus::Queued => {
                    task.cancel_requested.store(true, Ordering::Release);
                    *task.status.lock().await = BatchTaskStatus::Cancelled;
                }
                BatchTaskStatus::Running => {
                    task.manager.cancel().await;
                }
                BatchTaskStatus::Completed
                | BatchTaskStatus::Failed
                | BatchTaskStatus::Cancelled => {}
            }
        }
        self.refresh_batch(batch_id, &runtime).await;
        let snapshot = runtime.snapshot.lock().await.clone();
        Ok(snapshot)
    }

    pub async fn cancel_task(
        &self,
        batch_id: &str,
        task_id: &str,
    ) -> Result<BatchSnapshot, AppError> {
        let runtime = self
            .inner
            .batches
            .lock()
            .await
            .get(batch_id)
            .cloned()
            .ok_or(AppError::InvalidImageReference(batch_id.into()))?;
        let mut selected = None;
        for task in &runtime.tasks {
            if task.task_id == task_id {
                selected = Some(task.clone());
                break;
            }
        }
        let task = selected.ok_or(AppError::InvalidImageReference(task_id.into()))?;
        let status = *task.status.lock().await;
        if status == BatchTaskStatus::Queued {
            task.cancel_requested.store(true, Ordering::Release);
            *task.status.lock().await = BatchTaskStatus::Cancelled;
        } else if status == BatchTaskStatus::Running {
            task.manager.cancel().await;
        }
        self.refresh_batch(batch_id, &runtime).await;
        let snapshot = runtime.snapshot.lock().await.clone();
        Ok(snapshot)
    }

    pub async fn retry_task(
        &self,
        batch_id: &str,
        task_id: &str,
    ) -> Result<BatchSnapshot, AppError> {
        let runtime = self
            .inner
            .batches
            .lock()
            .await
            .get(batch_id)
            .cloned()
            .ok_or(AppError::InvalidImageReference(batch_id.into()))?;
        if runtime.cancellation.is_cancelled() {
            return Err(AppError::TaskAlreadyRunning);
        }
        let task = runtime
            .tasks
            .iter()
            .find(|task| task.task_id == task_id)
            .cloned()
            .ok_or(AppError::InvalidImageReference(task_id.into()))?;
        let status = *task.status.lock().await;
        if !matches!(status, BatchTaskStatus::Failed | BatchTaskStatus::Cancelled) {
            return Err(AppError::InternalError(
                "only failed or cancelled tasks can be retried".into(),
            ));
        }
        {
            let mut active = self.inner.active_batch.lock().await;
            if active
                .as_deref()
                .is_some_and(|active_id| active_id != batch_id)
            {
                return Err(AppError::TaskAlreadyRunning);
            }
            *active = Some(batch_id.to_owned());
        }
        task.cancel_requested.store(false, Ordering::Release);
        *task.status.lock().await = BatchTaskStatus::Queued;
        let manager = self.clone();
        let runtime_for_task = runtime.clone();
        let task_batch_id = batch_id.to_owned();
        tokio::spawn(async move {
            manager
                .run_task(
                    task_batch_id,
                    runtime_for_task.clone(),
                    task,
                    runtime_for_task.semaphore.clone(),
                )
                .await;
        });
        self.refresh_batch(batch_id, &runtime).await;
        let snapshot = runtime.snapshot.lock().await.clone();
        Ok(snapshot)
    }

    async fn refresh_batch(&self, batch_id: &str, runtime: &Arc<BatchRuntime>) {
        let task_snapshots = join_all(runtime.tasks.iter().map(task_snapshot)).await;
        let statuses: Vec<_> = join_all(
            runtime
                .tasks
                .iter()
                .map(|task| async { *task.status.lock().await }),
        )
        .await;
        let mut snapshot = runtime.snapshot.lock().await;
        snapshot.tasks = task_snapshots;
        if statuses
            .iter()
            .all(|status| matches!(status, BatchTaskStatus::Completed))
        {
            snapshot.status = BatchStatus::Completed;
        } else if runtime.cancellation.is_cancelled()
            && statuses.iter().all(|status| {
                matches!(
                    status,
                    BatchTaskStatus::Completed
                        | BatchTaskStatus::Failed
                        | BatchTaskStatus::Cancelled
                )
            })
        {
            snapshot.status = BatchStatus::Cancelled;
        } else if statuses
            .iter()
            .any(|status| matches!(status, BatchTaskStatus::Running))
        {
            snapshot.status = BatchStatus::Running;
        } else if statuses
            .iter()
            .any(|status| matches!(status, BatchTaskStatus::Completed))
            && statuses.iter().any(|status| {
                matches!(status, BatchTaskStatus::Failed | BatchTaskStatus::Cancelled)
            })
        {
            snapshot.status = BatchStatus::PartialFailed;
        } else if statuses
            .iter()
            .all(|status| matches!(status, BatchTaskStatus::Failed | BatchTaskStatus::Cancelled))
        {
            snapshot.status = if runtime.cancellation.is_cancelled() {
                BatchStatus::Cancelled
            } else {
                BatchStatus::Failed
            };
        } else {
            snapshot.status = BatchStatus::Queued;
        }
        let cloned = snapshot.clone();
        drop(snapshot);
        self.emit_snapshot(&cloned);
        if matches!(
            cloned.status,
            BatchStatus::Completed
                | BatchStatus::PartialFailed
                | BatchStatus::Failed
                | BatchStatus::Cancelled
        ) {
            let mut active = self.inner.active_batch.lock().await;
            if active.as_deref() == Some(batch_id) {
                *active = None;
            }
        }
    }

    fn emit_snapshot(&self, snapshot: &BatchSnapshot) {
        if let Ok(value) = to_value(snapshot) {
            self.inner.events.emit(BATCH_STATE_EVENT, value);
        }
    }
}

struct BatchTaskEventSink {
    inner: Arc<dyn EventSink>,
    task_id: String,
}

impl EventSink for BatchTaskEventSink {
    fn emit(&self, event: &'static str, mut payload: Value) {
        if matches!(
            event,
            DOWNLOAD_STATE_EVENT | DOWNLOAD_PROGRESS_EVENT | DOWNLOAD_LOG_EVENT
        ) {
            if let Some(object) = payload.as_object_mut() {
                object.insert("taskId".into(), json!(self.task_id));
            }
        }
        self.inner.emit(event, payload);
    }
}

fn request_image(request: &DownloadStartRequest) -> Result<ImageReference, AppError> {
    let separator = if request.tag.starts_with("sha256:") {
        '@'
    } else {
        ':'
    };
    parse_image_reference(
        &format!(
            "{}{}{}{}{}",
            request.mirror,
            if request.mirror.ends_with('/') {
                ""
            } else {
                "/"
            },
            request.package_name,
            separator,
            request.tag
        ),
        &request.mirror,
    )
}

async fn task_snapshot(task: &Arc<BatchTaskRuntime>) -> BatchTaskSnapshot {
    let mut download = task.manager.snapshot().await;
    download.task_id = task.task_id.clone();
    let status = *task.status.lock().await;
    BatchTaskSnapshot {
        task_id: task.task_id.clone(),
        image: task.image.clone(),
        status,
        download,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryCredentialStore;
    use crate::domain::{AppSettings, CompletedDownload, DownloadStartRequest};
    use crate::error::AppError;
    use crate::events::EventSink;
    use crate::storage::ConfigStore;
    use crate::task_manager::TaskContext;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    #[derive(Default)]
    struct RecordingEvents(StdMutex<Vec<(&'static str, Value)>>);

    impl EventSink for RecordingEvents {
        fn emit(&self, event: &'static str, payload: Value) {
            self.0.lock().unwrap().push((event, payload));
        }
    }

    struct TestPipeline {
        active: AtomicUsize,
        max_active: AtomicUsize,
        delay: Duration,
        failures: AsyncMutex<HashSet<String>>,
    }

    impl TestPipeline {
        fn new(delay: Duration, failures: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                delay,
                failures: AsyncMutex::new(
                    failures
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<HashSet<_>>(),
                ),
            }
        }
    }

    #[async_trait]
    impl DownloadPipeline for TestPipeline {
        async fn run(
            &self,
            request: DownloadStartRequest,
            context: TaskContext,
        ) -> Result<CompletedDownload, AppError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let result = async {
                context.transition(DownloadStatus::Downloading).await?;
                tokio::select! {
                    _ = tokio::time::sleep(self.delay) => {},
                    _ = context.cancelled() => return Err(AppError::TaskCancelled),
                }
                if self.failures.lock().await.remove(&request.package_name) {
                    return Err(AppError::DigestMismatch);
                }
                context.transition(DownloadStatus::Verifying).await?;
                context.transition(DownloadStatus::Packing).await?;
                Ok(CompletedDownload {
                    output_path: format!("{}.tar", request.package_name.replace('/', "_")),
                    size_bytes: 1,
                })
            }
            .await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    fn settings() -> AppSettings {
        AppSettings {
            default_output_dir: "C:/output".into(),
            default_arch: "amd64".into(),
            default_mirror: "registry.test".into(),
            download_workers: 4,
            last_package_name: None,
        }
    }

    fn request(package_name: &str) -> DownloadStartRequest {
        DownloadStartRequest {
            package_name: package_name.into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: "registry.test".into(),
            output_dir: "C:/output".into(),
            username: None,
            password: None,
        }
    }

    async fn manager(
        pipeline: Arc<TestPipeline>,
    ) -> (BatchManager, Arc<RecordingEvents>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigStore::open(
            dir.path().join("config.json"),
            settings(),
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        let events = Arc::new(RecordingEvents::default());
        let manager = BatchManager::new(config.clone(), pipeline, events.clone(), Arc::new(config));
        (manager, events, dir)
    }

    async fn wait_for_status(manager: &BatchManager, batch_id: &str, expected: BatchStatus) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager.status(batch_id).await.unwrap().status == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn accepts_ten_images_deduplicates_and_rejects_eleven() {
        let pipeline = Arc::new(TestPipeline::new(Duration::from_millis(20), []));
        let (manager, _events, _dir) = manager(pipeline).await;
        let ten = (0..10)
            .map(|index| request(&format!("library/image{index}")))
            .collect();
        let snapshot = manager
            .start(BatchStartRequest {
                images: ten,
                max_concurrent_images: 2,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.tasks.len(), 10);
        assert!(matches!(
            manager
                .start(BatchStartRequest {
                    images: (0..11)
                        .map(|index| request(&format!("library/image{index}")))
                        .collect(),
                    max_concurrent_images: 2,
                })
                .await,
            Err(AppError::InvalidImageReference(_))
        ));
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::Completed).await;
    }

    #[tokio::test]
    async fn one_active_batch_and_shared_concurrency_limit_are_enforced() {
        let pipeline = Arc::new(TestPipeline::new(Duration::from_millis(50), []));
        let (manager, _events, _dir) = manager(pipeline.clone()).await;
        let snapshot = manager
            .start(BatchStartRequest {
                images: (0..4)
                    .map(|index| request(&format!("library/image{index}")))
                    .collect(),
                max_concurrent_images: 2,
            })
            .await
            .unwrap();
        assert!(matches!(
            manager
                .start(BatchStartRequest {
                    images: vec![request("library/other")],
                    max_concurrent_images: 1,
                })
                .await,
            Err(AppError::TaskAlreadyRunning)
        ));
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::Completed).await;
        assert!(pipeline.max_active.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn failed_item_does_not_stop_other_items() {
        let pipeline = Arc::new(TestPipeline::new(
            Duration::from_millis(20),
            ["library/fail"],
        ));
        let (manager, _events, _dir) = manager(pipeline).await;
        let snapshot = manager
            .start(BatchStartRequest {
                images: vec![request("library/fail"), request("library/ok")],
                max_concurrent_images: 2,
            })
            .await
            .unwrap();
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::PartialFailed).await;
        let final_snapshot = manager.status(&snapshot.batch_id).await.unwrap();
        assert_eq!(
            final_snapshot
                .tasks
                .iter()
                .filter(|task| task.status == BatchTaskStatus::Completed)
                .count(),
            1
        );
        assert_eq!(
            final_snapshot
                .tasks
                .iter()
                .filter(|task| task.status == BatchTaskStatus::Failed)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn failed_item_can_be_retried_without_resetting_other_items() {
        let pipeline = Arc::new(TestPipeline::new(
            Duration::from_millis(20),
            ["library/fail"],
        ));
        let (manager, _events, _dir) = manager(pipeline).await;
        let snapshot = manager
            .start(BatchStartRequest {
                images: vec![request("library/fail"), request("library/ok")],
                max_concurrent_images: 2,
            })
            .await
            .unwrap();
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::PartialFailed).await;
        let failed_task = manager
            .status(&snapshot.batch_id)
            .await
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.status == BatchTaskStatus::Failed)
            .unwrap()
            .task_id;
        manager
            .retry_task(&snapshot.batch_id, &failed_task)
            .await
            .unwrap();
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::Completed).await;
    }

    #[tokio::test]
    async fn cancelling_batch_cancels_running_and_queued_items() {
        let pipeline = Arc::new(TestPipeline::new(Duration::from_secs(2), []));
        let (manager, _events, _dir) = manager(pipeline).await;
        let snapshot = manager
            .start(BatchStartRequest {
                images: (0..4)
                    .map(|index| request(&format!("library/image{index}")))
                    .collect(),
                max_concurrent_images: 1,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        manager.cancel(&snapshot.batch_id).await.unwrap();
        wait_for_status(&manager, &snapshot.batch_id, BatchStatus::Cancelled).await;
        let final_snapshot = manager.status(&snapshot.batch_id).await.unwrap();
        assert!(
            final_snapshot
                .tasks
                .iter()
                .all(|task| task.status == BatchTaskStatus::Cancelled)
        );
    }
}
