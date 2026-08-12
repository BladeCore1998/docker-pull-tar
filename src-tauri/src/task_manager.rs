use crate::domain::{
    CompletedDownload, DownloadProgress, DownloadSnapshot, DownloadStartRequest, DownloadStatus,
    HistoryItem, ImageReference, LogEvent,
};
use crate::error::AppError;
use crate::events::{
    DOWNLOAD_LOG_EVENT, DOWNLOAD_PROGRESS_EVENT, DOWNLOAD_STATE_EVENT, EventSink, TaskLogEvent,
    TaskProgressEvent,
};
use crate::image_ref::parse_image_reference;
use crate::storage::{HistoryAppendOutcome, HistoryWriter};
use async_trait::async_trait;
use chrono::Utc;
use futures_util::FutureExt;
use serde::Serialize;
use std::any::Any;
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Weak};
use std::time::Duration;
#[cfg(debug_assertions)]
use tokio::sync::Notify;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[async_trait]
pub trait DownloadPipeline: Send + Sync {
    async fn run(
        &self,
        request: DownloadStartRequest,
        context: TaskContext,
    ) -> Result<CompletedDownload, AppError>;
}

#[derive(Clone)]
pub struct TaskContext {
    task_id: String,
    manager: Weak<TaskManagerInner>,
    cancellation: CancellationToken,
    secrets: Arc<Vec<String>>,
}

impl TaskContext {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub async fn transition(&self, next: DownloadStatus) -> Result<bool, AppError> {
        if self.is_cancelled() {
            return Ok(false);
        }
        let Some(manager) = self.manager.upgrade() else {
            return Ok(false);
        };
        let active = manager.active.lock().await;
        if !matches_active(&active, &self.task_id) {
            return Ok(false);
        }
        let mut snapshot = manager.snapshot.write().await;
        if snapshot.task_id != self.task_id {
            return Ok(false);
        }
        if !legal_transition(snapshot.status, next) {
            return Err(AppError::InternalError(format!(
                "illegal download state transition: {:?} -> {:?}",
                snapshot.status, next
            )));
        }
        snapshot.status = next;
        emit_snapshot(manager.events.as_ref(), &snapshot);
        Ok(true)
    }

    pub async fn report_progress(&self, progress: DownloadProgress, layer_completed: bool) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        let mut active = manager.active.lock().await;
        if !matches_active(&active, &self.task_id) {
            return false;
        }
        let now = Instant::now();
        let should_emit = active.as_ref().is_some_and(|task| {
            layer_completed
                || task
                    .last_progress_emit
                    .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
        });
        let mut snapshot = manager.snapshot.write().await;
        if snapshot.task_id != self.task_id {
            return false;
        }
        snapshot.progress = progress.clone();
        if should_emit {
            if let Some(task) = active.as_mut() {
                task.last_progress_emit = Some(now);
            }
            let event = TaskProgressEvent {
                task_id: self.task_id.clone(),
                progress,
            };
            emit_serialized(manager.events.as_ref(), DOWNLOAD_PROGRESS_EVENT, &event);
        }
        true
    }

    pub async fn report_terminal_progress(&self, progress: DownloadProgress) -> bool {
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        let active = manager.active.lock().await;
        if !matches_active(&active, &self.task_id) {
            return false;
        }
        let mut snapshot = manager.snapshot.write().await;
        if snapshot.task_id != self.task_id {
            return false;
        }
        snapshot.progress = progress.clone();
        let event = TaskProgressEvent {
            task_id: self.task_id.clone(),
            progress,
        };
        emit_serialized(manager.events.as_ref(), DOWNLOAD_PROGRESS_EVENT, &event);
        true
    }

    pub async fn log(&self, level: impl Into<String>, message: impl AsRef<str>) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        let active = manager.active.lock().await;
        if !matches_active(&active, &self.task_id) {
            return false;
        }
        let event = LogEvent {
            level: level.into(),
            message: sanitize(message.as_ref(), &self.secrets),
            timestamp: Utc::now().to_rfc3339(),
        };
        let event = TaskLogEvent {
            task_id: self.task_id.clone(),
            log: event,
        };
        emit_serialized(manager.events.as_ref(), DOWNLOAD_LOG_EVENT, &event);
        true
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<TaskManagerInner>,
}

struct TaskManagerInner {
    snapshot: Arc<RwLock<DownloadSnapshot>>,
    active: Mutex<Option<ActiveTask>>,
    pipeline: Arc<dyn DownloadPipeline>,
    events: Arc<dyn EventSink>,
    history: Arc<dyn HistoryWriter>,
    pending_history: Mutex<VecDeque<HistoryItem>>,
    #[cfg(debug_assertions)]
    terminal_gate: Mutex<Option<TerminalGate>>,
}

#[cfg(debug_assertions)]
#[derive(Clone)]
struct TerminalGate {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

struct ActiveTask {
    task_id: String,
    cancellation: CancellationToken,
    handle: Option<JoinHandle<()>>,
    finalizing: bool,
    last_progress_emit: Option<Instant>,
}

impl TaskManager {
    pub fn new(
        pipeline: Arc<dyn DownloadPipeline>,
        events: Arc<dyn EventSink>,
        history: Arc<dyn HistoryWriter>,
    ) -> Self {
        Self {
            inner: Arc::new(TaskManagerInner {
                snapshot: Arc::new(RwLock::new(idle_snapshot())),
                active: Mutex::new(None),
                pipeline,
                events,
                history,
                pending_history: Mutex::new(VecDeque::new()),
                #[cfg(debug_assertions)]
                terminal_gate: Mutex::new(None),
            }),
        }
    }

    pub async fn start(&self, request: DownloadStartRequest) -> Result<String, AppError> {
        flush_pending_history(&self.inner, 2).await;
        let image = request_image(&request)?;
        let task_id = Uuid::new_v4().to_string();
        let cancellation = CancellationToken::new();
        let mut active = self.inner.active.lock().await;
        if active.is_some() {
            return Err(AppError::TaskAlreadyRunning);
        }
        *active = Some(ActiveTask {
            task_id: task_id.clone(),
            cancellation: cancellation.clone(),
            handle: None,
            finalizing: false,
            last_progress_emit: None,
        });

        let snapshot = DownloadSnapshot {
            task_id: task_id.clone(),
            image,
            status: DownloadStatus::Preparing,
            progress: DownloadProgress::default(),
            started_at: Some(Utc::now().to_rfc3339()),
            finished_at: None,
            last_error: None,
        };
        *self.inner.snapshot.write().await = snapshot.clone();
        emit_snapshot(self.inner.events.as_ref(), &snapshot);

        let context = TaskContext {
            task_id: task_id.clone(),
            manager: Arc::downgrade(&self.inner),
            cancellation,
            secrets: Arc::new(
                request
                    .password
                    .iter()
                    .filter(|secret| !secret.is_empty())
                    .cloned()
                    .collect(),
            ),
        };
        let inner = self.inner.clone();
        let spawned_task_id = task_id.clone();
        let handle = tokio::spawn(async move {
            run_pipeline(inner, spawned_task_id, request, context).await;
        });
        if let Some(task) = active.as_mut() {
            task.handle = Some(handle);
        }
        drop(active);
        Ok(task_id)
    }

    pub async fn cancel(&self) -> bool {
        let active = self.inner.active.lock().await;
        let Some(task) = active.as_ref() else {
            return false;
        };
        if task.finalizing {
            return false;
        }
        let _already_finished = task.handle.as_ref().is_some_and(JoinHandle::is_finished);
        task.cancellation.cancel();
        true
    }

    pub async fn snapshot(&self) -> DownloadSnapshot {
        flush_pending_history(&self.inner, 2).await;
        self.inner.snapshot.read().await.clone()
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn install_terminal_gate(&self, reached: Arc<Notify>, release: Arc<Notify>) {
        *self.inner.terminal_gate.lock().await = Some(TerminalGate { reached, release });
    }
}

async fn run_pipeline(
    inner: Arc<TaskManagerInner>,
    task_id: String,
    request: DownloadStartRequest,
    context: TaskContext,
) {
    let lifecycle = AssertUnwindSafe(run_lifecycle(
        inner.clone(),
        task_id.clone(),
        request.clone(),
        context.clone(),
    ))
    .catch_unwind()
    .await;
    if let Err(panic) = lifecycle {
        let error = format!(
            "download task lifecycle panicked: {}",
            sanitize(&panic_message(panic), &context.secrets)
        );
        reset_finalization(&inner, &task_id).await;
        finalize(inner, task_id, request, Terminal::failed(error)).await;
    }
}

async fn run_lifecycle(
    inner: Arc<TaskManagerInner>,
    task_id: String,
    request: DownloadStartRequest,
    context: TaskContext,
) {
    let result = AssertUnwindSafe(inner.pipeline.run(request.clone(), context.clone()))
        .catch_unwind()
        .await;
    let terminal = if context.is_cancelled() {
        Terminal::cancelled()
    } else {
        match result {
            Ok(Ok(completed)) => {
                let state = inner.snapshot.read().await;
                if state.task_id == task_id && state.status == DownloadStatus::Packing {
                    Terminal::completed(completed)
                } else {
                    Terminal::failed(
                        "internal error: pipeline completed before entering packing state".into(),
                    )
                }
            }
            Ok(Err(AppError::TaskCancelled)) => Terminal::cancelled(),
            Ok(Err(error)) => Terminal::failed(sanitize(&error.to_string(), &context.secrets)),
            Err(panic) => Terminal::failed(format!(
                "download pipeline panicked: {}",
                sanitize(&panic_message(panic), &context.secrets)
            )),
        }
    };
    #[cfg(debug_assertions)]
    wait_at_terminal_gate(&inner).await;
    finalize(inner, task_id, request, terminal).await;
}

#[cfg(debug_assertions)]
async fn wait_at_terminal_gate(inner: &TaskManagerInner) {
    let gate = inner.terminal_gate.lock().await.take();
    if let Some(gate) = gate {
        gate.reached.notify_one();
        gate.release.notified().await;
    }
}

async fn finalize(
    inner: Arc<TaskManagerInner>,
    task_id: String,
    request: DownloadStartRequest,
    mut terminal: Terminal,
) {
    {
        let mut active = inner.active.lock().await;
        let Some(task) = active.as_mut() else {
            return;
        };
        if task.task_id != task_id || task.finalizing {
            return;
        }
        if task.cancellation.is_cancelled() {
            terminal = Terminal::cancelled();
        }
        task.finalizing = true;
    }

    let finished_at = Utc::now().to_rfc3339();
    let item = terminal_history_item(&task_id, &request, &terminal, &finished_at);
    enqueue_history(&inner, item).await;
    flush_pending_history(&inner, 2).await;

    if terminal.status == DownloadStatus::Completed {
        let failed = Terminal::failed(history_persistence_error());
        let failed_item = terminal_history_item(&task_id, &request, &failed, &finished_at);
        if replace_pending_history(&inner, &task_id, failed_item).await {
            terminal = failed;
        }
    }

    let mut active = inner.active.lock().await;
    if active.as_ref().is_none_or(|task| task.task_id != task_id) {
        return;
    }
    let mut snapshot = inner.snapshot.write().await;
    if snapshot.task_id != task_id {
        return;
    }
    snapshot.status = terminal.status;
    snapshot.finished_at = Some(finished_at);
    snapshot.last_error = terminal.error;
    emit_snapshot(inner.events.as_ref(), &snapshot);
    active.take();
}

fn terminal_history_item(
    task_id: &str,
    request: &DownloadStartRequest,
    terminal: &Terminal,
    finished_at: &str,
) -> HistoryItem {
    HistoryItem {
        id: task_id.to_owned(),
        package_name: request.package_name.clone(),
        tag: request.tag.clone(),
        arch: request.arch.clone(),
        mirror: request.mirror.clone(),
        full_image: full_image(request),
        output_path: terminal
            .completed
            .as_ref()
            .map(|value| value.output_path.clone()),
        size: terminal
            .completed
            .as_ref()
            .map(|value| value.size_bytes.to_string()),
        status: terminal.status,
        downloaded_at: Some(finished_at.to_owned()),
        error: terminal.error.clone(),
    }
}

async fn reset_finalization(inner: &TaskManagerInner, task_id: &str) {
    let mut active = inner.active.lock().await;
    if let Some(task) = active.as_mut().filter(|task| task.task_id == task_id) {
        task.finalizing = false;
    }
}

async fn enqueue_history(inner: &TaskManagerInner, item: HistoryItem) {
    let mut pending = inner.pending_history.lock().await;
    if !pending.iter().any(|pending| pending.id == item.id) {
        pending.push_back(item);
    }
}

async fn replace_pending_history(
    inner: &TaskManagerInner,
    task_id: &str,
    replacement: HistoryItem,
) -> bool {
    let mut pending = inner.pending_history.lock().await;
    let Some(item) = pending.iter_mut().find(|item| item.id == task_id) else {
        return false;
    };
    *item = replacement;
    true
}

async fn flush_pending_history(inner: &TaskManagerInner, attempts: usize) {
    let mut pending = inner.pending_history.lock().await;
    for _ in 0..attempts {
        let Some(item) = pending.front().cloned() else {
            break;
        };
        if matches!(
            append_history_safely(inner.history.as_ref(), item).await,
            Ok(HistoryAppendOutcome::Committed | HistoryAppendOutcome::CommitIndeterminate)
        ) {
            pending.pop_front();
        }
    }
}

async fn append_history_safely(
    history: &dyn HistoryWriter,
    item: HistoryItem,
) -> Result<HistoryAppendOutcome, AppError> {
    match AssertUnwindSafe(history.append_history(item))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(_) => Err(AppError::ConfigIoFailed),
    }
}

fn history_persistence_error() -> String {
    let error = AppError::ConfigIoFailed;
    format!("{}: {error}", error.code())
}

struct Terminal {
    status: DownloadStatus,
    completed: Option<CompletedDownload>,
    error: Option<String>,
}

impl Terminal {
    fn completed(completed: CompletedDownload) -> Self {
        Self {
            status: DownloadStatus::Completed,
            completed: Some(completed),
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            status: DownloadStatus::Failed,
            completed: None,
            error: Some(error),
        }
    }

    fn cancelled() -> Self {
        Self {
            status: DownloadStatus::Cancelled,
            completed: None,
            error: None,
        }
    }
}

fn matches_active(active: &Option<ActiveTask>, task_id: &str) -> bool {
    active
        .as_ref()
        .is_some_and(|task| task.task_id == task_id && !task.finalizing)
}

fn legal_transition(current: DownloadStatus, next: DownloadStatus) -> bool {
    matches!(
        (current, next),
        (DownloadStatus::Preparing, DownloadStatus::Downloading)
            | (DownloadStatus::Downloading, DownloadStatus::Verifying)
            | (DownloadStatus::Verifying, DownloadStatus::Packing)
    )
}

fn request_image(request: &DownloadStartRequest) -> Result<ImageReference, AppError> {
    let image = if request.tag.starts_with("sha256:") {
        format!("{}@{}", request.package_name, request.tag)
    } else {
        format!("{}:{}", request.package_name, request.tag)
    };
    parse_image_reference(&image, &request.mirror)
}

fn full_image(request: &DownloadStartRequest) -> String {
    let separator = if request.tag.starts_with("sha256:") {
        '@'
    } else {
        ':'
    };
    format!(
        "{}/{}{}{}",
        request.mirror, request.package_name, separator, request.tag
    )
}

fn idle_snapshot() -> DownloadSnapshot {
    DownloadSnapshot {
        task_id: String::new(),
        image: ImageReference {
            registry: String::new(),
            repository: String::new(),
            reference: String::new(),
        },
        status: DownloadStatus::Idle,
        progress: DownloadProgress::default(),
        started_at: None,
        finished_at: None,
        last_error: None,
    }
}

fn emit_snapshot(events: &dyn EventSink, snapshot: &DownloadSnapshot) {
    emit_serialized(events, DOWNLOAD_STATE_EVENT, snapshot);
}

fn emit_serialized<T: Serialize>(events: &dyn EventSink, event: &'static str, payload: &T) {
    let Ok(payload) = serde_json::to_value(payload) else {
        return;
    };
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| events.emit(event, payload)));
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".into()
    }
}

fn sanitize(message: &str, secrets: &[String]) -> String {
    let mut sanitized = redact_authorization_headers(message);
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        sanitized = sanitized.replace(secret, "[REDACTED]");
    }
    for marker in ["Bearer ", "Basic ", "token=", "password="] {
        sanitized = redact_value_after(&sanitized, marker);
    }
    sanitized
}

fn redact_authorization_headers(input: &str) -> String {
    let marker = "authorization:";
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    loop {
        let lowercase = remaining.to_ascii_lowercase();
        let Some(index) = lowercase.find(marker) else {
            output.push_str(remaining);
            break;
        };
        let value_start = index + marker.len();
        output.push_str(&remaining[..value_start]);
        output.push_str(" [REDACTED]");
        let value_end = remaining[value_start..]
            .find(['\r', '\n'])
            .map_or(remaining.len(), |offset| value_start + offset);
        remaining = &remaining[value_end..];
    }
    output
}

fn redact_value_after(input: &str, marker: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    loop {
        let lowercase = remaining.to_ascii_lowercase();
        let marker_lowercase = marker.to_ascii_lowercase();
        let Some(index) = lowercase.find(&marker_lowercase) else {
            output.push_str(remaining);
            break;
        };
        let value_start = index + marker.len();
        output.push_str(&remaining[..value_start]);
        output.push_str("[REDACTED]");
        let value_end = remaining[value_start..]
            .find(char::is_whitespace)
            .map_or(remaining.len(), |offset| value_start + offset);
        remaining = &remaining[value_end..];
    }
    output
}
