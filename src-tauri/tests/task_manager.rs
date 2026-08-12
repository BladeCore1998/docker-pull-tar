use async_trait::async_trait;
use docker_pull_tar_lib::credentials::{Credential, CredentialStore};
use docker_pull_tar_lib::domain::{
    AppSettings, CompletedDownload, DownloadProgress, DownloadSnapshot, DownloadStartRequest,
    DownloadStatus, HistoryItem, MirrorInput, MirrorPatch,
};
use docker_pull_tar_lib::error::AppError;
use docker_pull_tar_lib::events::{
    DOWNLOAD_LOG_EVENT, DOWNLOAD_PROGRESS_EVENT, DOWNLOAD_STATE_EVENT, EventSink, TaskLogEvent,
    TaskProgressEvent,
};
use docker_pull_tar_lib::storage::{
    ConfigStore, HistoryAppendOutcome, HistoryWriter, StorageMaintenanceWarning,
};
use docker_pull_tar_lib::task_manager::{DownloadPipeline, TaskContext, TaskManager};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct RecordedEvent {
    name: &'static str,
    payload: Value,
}

#[derive(Default)]
struct RecordingSink {
    events: StdMutex<Vec<RecordedEvent>>,
}

impl RecordingSink {
    fn named(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.name == name)
            .map(|event| event.payload.clone())
            .collect()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, name: &'static str, payload: Value) {
        self.events
            .lock()
            .unwrap()
            .push(RecordedEvent { name, payload });
    }
}

struct PanickingSink;

impl EventSink for PanickingSink {
    fn emit(&self, _name: &'static str, _payload: Value) {
        panic!("event sink panic");
    }
}

#[derive(Default)]
struct RecordingHistory {
    rows: Mutex<Vec<HistoryItem>>,
}

#[async_trait]
impl HistoryWriter for RecordingHistory {
    async fn append_history(&self, item: HistoryItem) -> Result<HistoryAppendOutcome, AppError> {
        self.rows.lock().await.push(item);
        Ok(HistoryAppendOutcome::Committed)
    }
}

#[derive(Default)]
struct FaultyHistory {
    state: StdMutex<FaultyHistoryState>,
}

#[derive(Default)]
struct FaultyHistoryState {
    attempts: usize,
    failures_remaining: usize,
    persistent_failure: bool,
    panic_on_append: bool,
    rows: Vec<HistoryItem>,
}

impl FaultyHistory {
    fn fail_times(count: usize) -> Self {
        Self {
            state: StdMutex::new(FaultyHistoryState {
                failures_remaining: count,
                ..Default::default()
            }),
        }
    }

    fn persistent() -> Self {
        Self {
            state: StdMutex::new(FaultyHistoryState {
                persistent_failure: true,
                ..Default::default()
            }),
        }
    }

    fn panicking() -> Self {
        Self {
            state: StdMutex::new(FaultyHistoryState {
                panic_on_append: true,
                ..Default::default()
            }),
        }
    }

    fn recover(&self) {
        let mut state = self.state.lock().unwrap();
        state.persistent_failure = false;
        state.panic_on_append = false;
    }

    fn attempts(&self) -> usize {
        self.state.lock().unwrap().attempts
    }

    fn rows(&self) -> Vec<HistoryItem> {
        self.state.lock().unwrap().rows.clone()
    }
}

#[async_trait]
impl HistoryWriter for FaultyHistory {
    async fn append_history(&self, item: HistoryItem) -> Result<HistoryAppendOutcome, AppError> {
        let mut state = self.state.lock().unwrap();
        state.attempts += 1;
        if state.panic_on_append {
            drop(state);
            panic!("history writer panic");
        }
        if state.persistent_failure {
            return Err(AppError::ConfigIoFailed);
        }
        if state.failures_remaining > 0 {
            state.failures_remaining -= 1;
            return Err(AppError::ConfigIoFailed);
        }
        if !state.rows.iter().any(|row| row.id == item.id) {
            state.rows.push(item);
        }
        Ok(HistoryAppendOutcome::Committed)
    }
}

struct CleanupFailureCredentials {
    values: Mutex<HashMap<String, Credential>>,
    delete_calls: Mutex<usize>,
    fail_next_get: Mutex<bool>,
    config_path: PathBuf,
}

impl CleanupFailureCredentials {
    fn new(config_path: PathBuf) -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            delete_calls: Mutex::new(0),
            fail_next_get: Mutex::new(false),
            config_path,
        }
    }

    async fn delete_calls(&self) -> usize {
        *self.delete_calls.lock().await
    }
}

#[async_trait]
impl CredentialStore for CleanupFailureCredentials {
    async fn get(&self, key: &str) -> Result<Option<Credential>, AppError> {
        let mut fail = self.fail_next_get.lock().await;
        if *fail {
            *fail = false;
            return Err(AppError::CredentialStoreFailed);
        }
        Ok(self.values.lock().await.get(key).cloned())
    }

    async fn set(&self, key: &str, value: &Credential) -> Result<(), AppError> {
        self.values.lock().await.insert(key.into(), value.clone());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), AppError> {
        let mut calls = self.delete_calls.lock().await;
        *calls += 1;
        self.values.lock().await.remove(key);
        if *calls == 1 {
            *self.fail_next_get.lock().await = true;
            return Err(AppError::CredentialStoreFailed);
        }
        if *calls == 2 {
            tokio::fs::remove_file(&self.config_path)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
            tokio::fs::create_dir(&self.config_path)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
            return Err(AppError::CredentialStoreFailed);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Finish {
    Complete,
    Error,
    Panic,
}

struct FakePipeline {
    phases: Vec<DownloadStatus>,
    hold_at: Option<DownloadStatus>,
    finish: Finish,
    reached: Notify,
    release: Notify,
    captured: Mutex<Option<TaskContext>>,
}

impl FakePipeline {
    fn completing() -> Self {
        Self::new(
            vec![
                DownloadStatus::Downloading,
                DownloadStatus::Verifying,
                DownloadStatus::Packing,
            ],
            Some(DownloadStatus::Packing),
            Finish::Complete,
        )
    }

    fn new(phases: Vec<DownloadStatus>, hold_at: Option<DownloadStatus>, finish: Finish) -> Self {
        Self {
            phases,
            hold_at,
            finish,
            reached: Notify::new(),
            release: Notify::new(),
            captured: Mutex::new(None),
        }
    }

    async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }

    async fn context(&self) -> TaskContext {
        loop {
            if let Some(context) = self.captured.lock().await.clone() {
                return context;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait]
impl DownloadPipeline for FakePipeline {
    async fn run(
        &self,
        _request: DownloadStartRequest,
        context: TaskContext,
    ) -> Result<CompletedDownload, AppError> {
        *self.captured.lock().await = Some(context.clone());
        if self.hold_at == Some(DownloadStatus::Preparing) {
            self.reached.notify_one();
            context.cancelled().await;
            return Err(AppError::TaskCancelled);
        }
        for phase in &self.phases {
            context.transition(*phase).await?;
            if self.hold_at == Some(*phase) {
                self.reached.notify_one();
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = context.cancelled() => return Err(AppError::TaskCancelled),
                }
            }
        }
        match self.finish {
            Finish::Complete => Ok(CompletedDownload {
                output_path: "C:/output/nginx.tar".into(),
                size_bytes: 4096,
            }),
            Finish::Error => Err(AppError::DigestMismatch),
            Finish::Panic => panic!("pipeline panic"),
        }
    }
}

struct SwitchingPipeline {
    current: StdMutex<Arc<dyn DownloadPipeline>>,
}

impl SwitchingPipeline {
    fn new(pipeline: Arc<dyn DownloadPipeline>) -> Self {
        Self {
            current: StdMutex::new(pipeline),
        }
    }

    fn set(&self, pipeline: Arc<dyn DownloadPipeline>) {
        *self.current.lock().unwrap() = pipeline;
    }
}

#[async_trait]
impl DownloadPipeline for SwitchingPipeline {
    async fn run(
        &self,
        request: DownloadStartRequest,
        context: TaskContext,
    ) -> Result<CompletedDownload, AppError> {
        let pipeline = self.current.lock().unwrap().clone();
        pipeline.run(request, context).await
    }
}

fn request(password: Option<&str>) -> DownloadStartRequest {
    DownloadStartRequest {
        package_name: "library/nginx".into(),
        tag: "latest".into(),
        arch: "amd64".into(),
        mirror: "docker.io".into(),
        output_dir: "C:/output".into(),
        username: Some("alice".into()),
        password: password.map(str::to_owned),
    }
}

fn settings() -> AppSettings {
    AppSettings {
        default_output_dir: "C:/output".into(),
        default_arch: "amd64".into(),
        default_mirror: "docker.io".into(),
        download_workers: 4,
        last_package_name: None,
    }
}

fn manager(
    pipeline: Arc<FakePipeline>,
) -> (TaskManager, Arc<RecordingSink>, Arc<RecordingHistory>) {
    let events = Arc::new(RecordingSink::default());
    let history = Arc::new(RecordingHistory::default());
    let manager = TaskManager::new(pipeline, events.clone(), history.clone());
    (manager, events, history)
}

async fn wait_for_status(manager: &TaskManager, expected: DownloadStatus) -> DownloadSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager.snapshot().await;
            if snapshot.status == expected {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn publishes_legal_order_and_completes_only_after_archive_result() {
    let pipeline = Arc::new(FakePipeline::completing());
    let (manager, events, history) = manager(pipeline.clone());
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;

    assert_eq!(manager.snapshot().await.status, DownloadStatus::Packing);
    assert!(history.rows.lock().await.is_empty());
    assert!(
        !events
            .named(DOWNLOAD_STATE_EVENT)
            .iter()
            .any(|payload| { payload["status"] == serde_json::json!("completed") })
    );

    pipeline.release();
    wait_for_status(&manager, DownloadStatus::Completed).await;
    let states: Vec<DownloadStatus> = events
        .named(DOWNLOAD_STATE_EVENT)
        .into_iter()
        .map(|value| {
            serde_json::from_value::<DownloadSnapshot>(value)
                .unwrap()
                .status
        })
        .collect();
    assert_eq!(
        states,
        vec![
            DownloadStatus::Preparing,
            DownloadStatus::Downloading,
            DownloadStatus::Verifying,
            DownloadStatus::Packing,
            DownloadStatus::Completed,
        ]
    );
    let rows = history.rows.lock().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, DownloadStatus::Completed);
    assert_eq!(rows[0].output_path.as_deref(), Some("C:/output/nginx.tar"));
}

#[tokio::test]
async fn simultaneous_starts_reserve_exactly_one_task() {
    let pipeline = Arc::new(FakePipeline::new(
        vec![],
        Some(DownloadStatus::Preparing),
        Finish::Complete,
    ));
    let (manager, _, _) = manager(pipeline);
    let first = manager.clone();
    let second = manager.clone();
    let (one, two) = tokio::join!(first.start(request(None)), second.start(request(None)));
    let results = [one, two];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(AppError::TaskAlreadyRunning)))
            .count(),
        1
    );
    assert!(manager.cancel().await);
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn cancellation_is_terminal_in_every_active_phase() {
    for phase in [
        DownloadStatus::Preparing,
        DownloadStatus::Downloading,
        DownloadStatus::Verifying,
        DownloadStatus::Packing,
    ] {
        let phases = match phase {
            DownloadStatus::Preparing => vec![],
            DownloadStatus::Downloading => vec![DownloadStatus::Downloading],
            DownloadStatus::Verifying => {
                vec![DownloadStatus::Downloading, DownloadStatus::Verifying]
            }
            DownloadStatus::Packing => vec![
                DownloadStatus::Downloading,
                DownloadStatus::Verifying,
                DownloadStatus::Packing,
            ],
            _ => unreachable!(),
        };
        let pipeline = Arc::new(FakePipeline::new(phases, Some(phase), Finish::Complete));
        let (manager, _, history) = manager(pipeline.clone());
        manager.start(request(None)).await.unwrap();
        pipeline.wait_until_reached().await;
        assert!(manager.cancel().await, "cancel rejected in {phase:?}");
        wait_for_status(&manager, DownloadStatus::Cancelled).await;
        let rows = history.rows.lock().await;
        assert_eq!(rows.len(), 1, "wrong history count in {phase:?}");
        assert_eq!(rows[0].status, DownloadStatus::Cancelled);
    }
}

#[tokio::test]
async fn errors_and_panics_finalize_failed_once_and_release_the_slot() {
    for finish in [Finish::Error, Finish::Panic] {
        let pipeline = Arc::new(FakePipeline::new(vec![], None, finish));
        let (manager, _, history) = manager(pipeline);
        manager.start(request(None)).await.unwrap();
        let failed = wait_for_status(&manager, DownloadStatus::Failed).await;
        assert!(failed.last_error.is_some());
        assert_eq!(history.rows.lock().await.len(), 1);

        assert!(manager.start(request(None)).await.is_ok());
        wait_for_status(&manager, DownloadStatus::Failed).await;
        assert_eq!(history.rows.lock().await.len(), 2);
    }
}

#[tokio::test]
async fn state_event_payload_equals_the_query_snapshot() {
    let pipeline = Arc::new(FakePipeline::completing());
    let (manager, events, _) = manager(pipeline.clone());
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    let queried = manager.snapshot().await;
    let emitted: DownloadSnapshot =
        serde_json::from_value(events.named(DOWNLOAD_STATE_EVENT).last().unwrap().clone()).unwrap();
    assert_eq!(queried, emitted);
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn stale_task_context_cannot_mutate_a_newer_snapshot() {
    let old_pipeline = Arc::new(FakePipeline::new(vec![], None, Finish::Error));
    let switch = Arc::new(SwitchingPipeline::new(old_pipeline.clone()));
    let events = Arc::new(RecordingSink::default());
    let history = Arc::new(RecordingHistory::default());
    let manager = TaskManager::new(switch.clone(), events.clone(), history);
    manager.start(request(None)).await.unwrap();
    let old_context = old_pipeline.context().await;
    wait_for_status(&manager, DownloadStatus::Failed).await;

    let new_pipeline = Arc::new(FakePipeline::new(
        vec![],
        Some(DownloadStatus::Preparing),
        Finish::Complete,
    ));
    switch.set(new_pipeline.clone());
    let new_id = manager.start(request(None)).await.unwrap();
    new_pipeline.wait_until_reached().await;
    assert!(
        !old_context
            .transition(DownloadStatus::Downloading)
            .await
            .unwrap()
    );
    assert!(
        !old_context
            .report_progress(
                DownloadProgress {
                    downloaded_bytes: 99,
                    ..DownloadProgress::default()
                },
                false
            )
            .await
    );
    let log_count = events.named(DOWNLOAD_LOG_EVENT).len();
    assert!(!old_context.log("info", "late event").await);
    assert_eq!(events.named(DOWNLOAD_LOG_EVENT).len(), log_count);
    let snapshot = manager.snapshot().await;
    assert_eq!(snapshot.task_id, new_id);
    assert_eq!(snapshot.status, DownloadStatus::Preparing);
    assert_eq!(snapshot.progress.downloaded_bytes, 0);
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test(start_paused = true)]
async fn progress_is_throttled_to_250ms_except_layer_completion() {
    let pipeline = Arc::new(FakePipeline::new(
        vec![],
        Some(DownloadStatus::Preparing),
        Finish::Complete,
    ));
    let (manager, events, _) = manager(pipeline.clone());
    let task_id = manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    let context = pipeline.context().await;

    for downloaded in [1, 2] {
        context
            .report_progress(
                DownloadProgress {
                    downloaded_bytes: downloaded,
                    ..DownloadProgress::default()
                },
                false,
            )
            .await;
    }
    assert_eq!(events.named(DOWNLOAD_PROGRESS_EVENT).len(), 1);
    tokio::time::advance(Duration::from_millis(249)).await;
    context
        .report_progress(
            DownloadProgress {
                downloaded_bytes: 3,
                ..DownloadProgress::default()
            },
            false,
        )
        .await;
    assert_eq!(events.named(DOWNLOAD_PROGRESS_EVENT).len(), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    context
        .report_progress(
            DownloadProgress {
                downloaded_bytes: 4,
                ..DownloadProgress::default()
            },
            false,
        )
        .await;
    context
        .report_progress(
            DownloadProgress {
                downloaded_bytes: 5,
                ..DownloadProgress::default()
            },
            true,
        )
        .await;
    assert_eq!(events.named(DOWNLOAD_PROGRESS_EVENT).len(), 3);
    for payload in events.named(DOWNLOAD_PROGRESS_EVENT) {
        let event: TaskProgressEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.task_id, task_id);
    }
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn log_events_redact_request_and_authorization_secrets() {
    let pipeline = Arc::new(FakePipeline::new(
        vec![],
        Some(DownloadStatus::Preparing),
        Finish::Complete,
    ));
    let (manager, events, _) = manager(pipeline.clone());
    let task_id = manager.start(request(Some("hunter2"))).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline
        .context()
        .await
        .log(
            "info",
            "password=hunter2 Authorization: Bearer token-secret token=another-secret",
        )
        .await;
    let payload = events.named(DOWNLOAD_LOG_EVENT)[0].clone();
    let event: TaskLogEvent = serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(event.task_id, task_id);
    let encoded = payload.to_string();
    assert!(!encoded.contains("hunter2"));
    assert!(!encoded.contains("token-secret"));
    assert!(!encoded.contains("another-secret"));
    assert!(encoded.contains("[REDACTED]"));
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn custom_authorization_scheme_is_redacted_through_the_line_boundary() {
    let pipeline = Arc::new(FakePipeline::new(
        vec![],
        Some(DownloadStatus::Preparing),
        Finish::Complete,
    ));
    let (manager, events, _) = manager(pipeline.clone());
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline
        .context()
        .await
        .log(
            "info",
            "AUTHORIZATION: ApiKey custom secret with spaces\r\nvisible: yes",
        )
        .await;
    let encoded = events.named(DOWNLOAD_LOG_EVENT)[0].to_string();
    assert!(!encoded.contains("ApiKey"));
    assert!(!encoded.contains("custom secret with spaces"));
    assert!(encoded.contains("visible: yes"));
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn pipeline_success_before_packing_is_failed_and_recorded_once() {
    let pipeline = Arc::new(FakePipeline::new(vec![], None, Finish::Complete));
    let (manager, _, history) = manager(pipeline);
    manager.start(request(None)).await.unwrap();
    let snapshot = wait_for_status(&manager, DownloadStatus::Failed).await;
    assert!(snapshot.last_error.as_deref().unwrap().contains("packing"));
    let rows = history.rows.lock().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, DownloadStatus::Failed);
    assert_eq!(rows[0].output_path, None);
}

#[tokio::test]
async fn panicking_event_sink_cannot_stick_the_task_or_context_calls() {
    let pipeline = Arc::new(FakePipeline::completing());
    let history = Arc::new(RecordingHistory::default());
    let manager = TaskManager::new(pipeline.clone(), Arc::new(PanickingSink), history.clone());
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    let context = pipeline.context().await;
    assert!(context.log("info", "still running").await);
    assert!(
        context
            .report_progress(DownloadProgress::default(), false)
            .await
    );
    pipeline.release();
    wait_for_status(&manager, DownloadStatus::Completed).await;
    assert_eq!(history.rows.lock().await.len(), 1);

    assert!(manager.start(request(None)).await.is_ok());
    pipeline.wait_until_reached().await;
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
}

#[tokio::test]
async fn transient_history_failure_is_retried_without_duplicate() {
    let pipeline = Arc::new(FakePipeline::completing());
    let history = Arc::new(FaultyHistory::fail_times(1));
    let manager = TaskManager::new(
        pipeline.clone(),
        Arc::new(RecordingSink::default()),
        history.clone(),
    );
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline.release();
    wait_for_status(&manager, DownloadStatus::Completed).await;
    assert_eq!(history.attempts(), 2);
    assert_eq!(history.rows().len(), 1);
}

#[tokio::test]
async fn persistent_history_failure_recovers_from_outbox_on_status_query() {
    let pipeline = Arc::new(FakePipeline::completing());
    let history = Arc::new(FaultyHistory::persistent());
    let events = Arc::new(RecordingSink::default());
    let manager = TaskManager::new(pipeline.clone(), events.clone(), history.clone());
    let task_id = manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline.release();
    let failed = wait_for_status(&manager, DownloadStatus::Failed).await;
    assert!(
        failed
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("CONFIG_IO_FAILED:")
    );
    assert!(
        !events
            .named(DOWNLOAD_STATE_EVENT)
            .iter()
            .any(|payload| { payload["taskId"] == task_id && payload["status"] == "completed" })
    );
    assert!(history.rows().is_empty());

    history.recover();
    manager.snapshot().await;
    let rows = history.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, task_id);
    assert_eq!(rows[0].status, DownloadStatus::Failed);
    assert_eq!(rows[0].output_path, None);
    assert!(
        rows[0]
            .error
            .as_deref()
            .unwrap()
            .starts_with("CONFIG_IO_FAILED:")
    );
    manager.snapshot().await;
    assert_eq!(history.rows().len(), 1);
}

#[tokio::test]
async fn panicking_history_writer_is_queued_and_next_task_is_accepted() {
    let pipeline = Arc::new(FakePipeline::completing());
    let history = Arc::new(FaultyHistory::panicking());
    let manager = TaskManager::new(
        pipeline.clone(),
        Arc::new(RecordingSink::default()),
        history.clone(),
    );
    let first_id = manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline.release();
    let failed = wait_for_status(&manager, DownloadStatus::Failed).await;
    assert!(
        failed
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("CONFIG_IO_FAILED:")
    );

    let second_id = manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    manager.cancel().await;
    wait_for_status(&manager, DownloadStatus::Cancelled).await;
    assert!(history.rows().is_empty());

    history.recover();
    manager.snapshot().await;
    let rows = history.rows();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.id == first_id));
    assert!(rows.iter().any(|row| row.id == second_id));
    let first = rows.iter().find(|row| row.id == first_id).unwrap();
    assert_eq!(first.status, DownloadStatus::Failed);
    assert_eq!(first.output_path, None);
}

#[tokio::test]
async fn config_history_commit_does_not_run_queued_credential_cleanup() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.json");
    let credentials = Arc::new(CleanupFailureCredentials::new(config_path.clone()));
    let store = Arc::new(
        ConfigStore::open(config_path.clone(), settings(), credentials.clone())
            .await
            .unwrap(),
    );
    let mirror = store
        .add_mirror(MirrorInput {
            name: "Docker".into(),
            registry: "docker.io".into(),
            is_default: true,
            username: Some("alice".into()),
            password: Some("secret".into()),
        })
        .await
        .unwrap();
    let before_update: Value =
        serde_json::from_slice(&tokio::fs::read(&config_path).await.unwrap()).unwrap();
    let old_ref = before_update["mirrors"][0]["credentialRef"]
        .as_str()
        .unwrap()
        .to_owned();
    let updated = store
        .update_mirror(
            &mirror.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.registry, "registry.example.com");
    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    let queued: Value =
        serde_json::from_slice(&tokio::fs::read(&config_path).await.unwrap()).unwrap();
    assert_eq!(queued["pendingCredentialCleanup"][0], old_ref);
    assert_ne!(queued["mirrors"][0]["credentialRef"], old_ref);
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
    assert_eq!(credentials.delete_calls().await, 1);

    let pipeline = Arc::new(FakePipeline::completing());
    let manager = TaskManager::new(
        pipeline.clone(),
        Arc::new(RecordingSink::default()),
        store.clone(),
    );
    let task_id = manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline.release();
    wait_for_status(&manager, DownloadStatus::Completed).await;

    assert_eq!(credentials.delete_calls().await, 1);
    assert!(config_path.is_file());
    let rows = store.history().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, task_id);
    assert_eq!(rows[0].status, DownloadStatus::Completed);
}

#[tokio::test]
async fn accepted_cancel_racing_pipeline_return_always_wins_terminal_selection() {
    let pipeline = Arc::new(FakePipeline::completing());
    let history = Arc::new(RecordingHistory::default());
    let manager = TaskManager::new(
        pipeline.clone(),
        Arc::new(RecordingSink::default()),
        history,
    );
    let terminal_selected = Arc::new(Notify::new());
    let allow_finalization = Arc::new(Notify::new());
    manager
        .install_terminal_gate(terminal_selected.clone(), allow_finalization.clone())
        .await;
    manager.start(request(None)).await.unwrap();
    pipeline.wait_until_reached().await;
    pipeline.release();
    terminal_selected.notified().await;
    let accepted = manager.cancel().await;
    allow_finalization.notify_one();
    let terminal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager.snapshot().await;
            if matches!(
                snapshot.status,
                DownloadStatus::Completed | DownloadStatus::Cancelled
            ) {
                return snapshot.status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(accepted);
    assert_eq!(terminal, DownloadStatus::Cancelled);
}

#[test]
fn tauri_bundle_icon_points_to_an_existing_file() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let icon = config["bundle"]["icon"][0].as_str().unwrap();
    assert!(
        manifest_dir.join(icon).is_file(),
        "missing bundle icon: {icon}"
    );
}

#[test]
fn release_profile_requires_unwind_for_task_panic_convergence() {
    let design =
        include_str!("../../docs/superpowers/specs/2026-07-12-tauri-rust-windows-client-design.md");
    let plan = include_str!("../../docs/superpowers/plans/2026-07-12-tauri-rust-windows-client.md");
    for document in [design, plan] {
        assert!(document.contains("panic = \"unwind\""));
        assert!(!document.contains("panic = \"abort\""));
    }
}
