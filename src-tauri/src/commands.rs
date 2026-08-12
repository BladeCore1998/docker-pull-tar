use crate::domain::{
    AppSettings, DownloadSnapshot, DownloadStartRequest, HistoryItem, Mirror, MirrorInput,
    MirrorPatch, SettingsPatch,
};
use crate::download_pipeline::DownloadService;
use crate::error::{AppError, CommandError};
use crate::storage::ConfigStore;
use async_trait::async_trait;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    pub success: bool,
}

impl CommandResult {
    fn success() -> Self {
        Self { success: true }
    }
}

#[async_trait]
pub trait SystemActions: Send + Sync {
    async fn select_directory(
        &self,
        initial_directory: Option<String>,
    ) -> Result<Option<String>, AppError>;

    fn open_directory(&self, path: &Path) -> Result<(), AppError>;
}

pub struct TauriSystemActions {
    app: AppHandle,
}

impl TauriSystemActions {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl SystemActions for TauriSystemActions {
    async fn select_directory(
        &self,
        initial_directory: Option<String>,
    ) -> Result<Option<String>, AppError> {
        let mut dialog = self.app.dialog().file();
        if let Some(initial_directory) = initial_directory.filter(|path| !path.trim().is_empty()) {
            dialog = dialog.set_directory(initial_directory);
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        dialog.pick_folder(move |selection| {
            let selected = selection
                .and_then(|path| path.into_path().ok())
                .map(|path| path.to_string_lossy().into_owned());
            let _ = sender.send(selected);
        });
        receiver
            .await
            .map_err(|_| AppError::InternalError("directory dialog closed unexpectedly".into()))
    }

    fn open_directory(&self, path: &Path) -> Result<(), AppError> {
        tauri_plugin_opener::open_path(path, None::<&str>)
            .map_err(|_| AppError::InternalError("failed to open directory".into()))
    }
}

pub struct AppState {
    pub config: ConfigStore,
    downloads: Arc<dyn DownloadService>,
    system: Arc<dyn SystemActions>,
}

impl AppState {
    pub fn new(
        config: ConfigStore,
        downloads: Arc<dyn DownloadService>,
        system: Arc<dyn SystemActions>,
    ) -> Self {
        Self {
            config,
            downloads,
            system,
        }
    }
}

pub async fn download_start_impl(
    state: &AppState,
    request: DownloadStartRequest,
) -> Result<DownloadSnapshot, CommandError> {
    state
        .downloads
        .start(request)
        .await
        .map_err(CommandError::from)
}

pub async fn download_cancel_impl(state: &AppState) -> Result<DownloadSnapshot, CommandError> {
    Ok(state.downloads.cancel().await)
}

pub async fn download_status_impl(state: &AppState) -> Result<DownloadSnapshot, CommandError> {
    Ok(state.downloads.status().await)
}

pub async fn settings_get_impl(state: &AppState) -> Result<AppSettings, CommandError> {
    Ok(state.config.settings().await)
}

pub async fn settings_update_impl(
    state: &AppState,
    patch: SettingsPatch,
) -> Result<AppSettings, CommandError> {
    state
        .config
        .update_settings(patch)
        .await
        .map_err(CommandError::from)
}

pub async fn mirrors_list_impl(state: &AppState) -> Result<Vec<Mirror>, CommandError> {
    state.config.mirrors().await.map_err(CommandError::from)
}

pub async fn mirror_add_impl(state: &AppState, input: MirrorInput) -> Result<Mirror, CommandError> {
    state
        .config
        .add_mirror(input)
        .await
        .map_err(CommandError::from)
}

pub async fn mirror_update_impl(
    state: &AppState,
    id: String,
    patch: MirrorPatch,
) -> Result<Mirror, CommandError> {
    state
        .config
        .update_mirror(&id, patch)
        .await
        .map_err(CommandError::from)
}

pub async fn mirror_delete_impl(
    state: &AppState,
    id: String,
) -> Result<CommandResult, CommandError> {
    state
        .config
        .delete_mirror(&id)
        .await
        .map_err(CommandError::from)?;
    Ok(CommandResult::success())
}

pub async fn mirror_set_default_impl(
    state: &AppState,
    id: String,
) -> Result<AppSettings, CommandError> {
    state
        .config
        .set_default_mirror(&id)
        .await
        .map_err(CommandError::from)?;
    Ok(state.config.settings().await)
}

pub async fn history_list_impl(state: &AppState) -> Result<Vec<HistoryItem>, CommandError> {
    Ok(state.config.history().await)
}

pub async fn history_delete_impl(
    state: &AppState,
    id: String,
) -> Result<CommandResult, CommandError> {
    state
        .config
        .delete_history(&id)
        .await
        .map_err(CommandError::from)?;
    Ok(CommandResult::success())
}

pub async fn history_clear_impl(state: &AppState) -> Result<CommandResult, CommandError> {
    state
        .config
        .clear_history()
        .await
        .map_err(CommandError::from)?;
    Ok(CommandResult::success())
}

pub async fn dialog_select_directory_impl(
    state: &AppState,
    initial_directory: Option<String>,
) -> Result<Option<String>, CommandError> {
    state
        .system
        .select_directory(initial_directory)
        .await
        .map_err(CommandError::from)
}

pub async fn path_open_parent_impl(
    state: &AppState,
    path: String,
) -> Result<CommandResult, CommandError> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(CommandError::from(AppError::OutputNotWritable));
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| CommandError::from(AppError::OutputNotWritable))?;
    let parent = canonical
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| CommandError::from(AppError::OutputNotWritable))?;
    state
        .system
        .open_directory(parent)
        .map_err(CommandError::from)?;
    Ok(CommandResult::success())
}

#[tauri::command]
pub async fn download_start(
    state: tauri::State<'_, AppState>,
    request: DownloadStartRequest,
) -> Result<DownloadSnapshot, CommandError> {
    download_start_impl(&state, request).await
}

#[tauri::command]
pub async fn download_cancel(
    state: tauri::State<'_, AppState>,
) -> Result<DownloadSnapshot, CommandError> {
    download_cancel_impl(&state).await
}

#[tauri::command]
pub async fn download_status(
    state: tauri::State<'_, AppState>,
) -> Result<DownloadSnapshot, CommandError> {
    download_status_impl(&state).await
}

#[tauri::command]
pub async fn settings_get(state: tauri::State<'_, AppState>) -> Result<AppSettings, CommandError> {
    settings_get_impl(&state).await
}

#[tauri::command]
pub async fn settings_update(
    state: tauri::State<'_, AppState>,
    patch: SettingsPatch,
) -> Result<AppSettings, CommandError> {
    settings_update_impl(&state, patch).await
}

#[tauri::command]
pub async fn mirrors_list(state: tauri::State<'_, AppState>) -> Result<Vec<Mirror>, CommandError> {
    mirrors_list_impl(&state).await
}

#[tauri::command]
pub async fn mirror_add(
    state: tauri::State<'_, AppState>,
    input: MirrorInput,
) -> Result<Mirror, CommandError> {
    mirror_add_impl(&state, input).await
}

#[tauri::command]
pub async fn mirror_update(
    state: tauri::State<'_, AppState>,
    id: String,
    patch: MirrorPatch,
) -> Result<Mirror, CommandError> {
    mirror_update_impl(&state, id, patch).await
}

#[tauri::command]
pub async fn mirror_delete(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<CommandResult, CommandError> {
    mirror_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn mirror_set_default(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<AppSettings, CommandError> {
    mirror_set_default_impl(&state, id).await
}

#[tauri::command]
pub async fn history_list(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<HistoryItem>, CommandError> {
    history_list_impl(&state).await
}

#[tauri::command]
pub async fn history_delete(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<CommandResult, CommandError> {
    history_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn history_clear(
    state: tauri::State<'_, AppState>,
) -> Result<CommandResult, CommandError> {
    history_clear_impl(&state).await
}

#[tauri::command]
pub async fn dialog_select_directory(
    state: tauri::State<'_, AppState>,
    initial_directory: Option<String>,
) -> Result<Option<String>, CommandError> {
    dialog_select_directory_impl(&state, initial_directory).await
}

#[tauri::command]
pub async fn path_open_parent(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<CommandResult, CommandError> {
    path_open_parent_impl(&state, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{Credential, CredentialStore, MemoryCredentialStore};
    use crate::domain::{CompletedDownload, DownloadStatus, HistoryItem, MirrorPatch};
    use crate::download_pipeline::DownloadCoordinator;
    use crate::events::EventSink;
    use crate::task_manager::{DownloadPipeline, TaskContext, TaskManager};
    use serde_json::Value;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, Notify};

    struct SilentEvents;

    impl EventSink for SilentEvents {
        fn emit(&self, _event: &'static str, _payload: Value) {}
    }

    struct UnavailableCredentials;

    #[async_trait]
    impl CredentialStore for UnavailableCredentials {
        async fn get(&self, _credential_ref: &str) -> Result<Option<Credential>, AppError> {
            Err(AppError::CredentialStoreFailed)
        }

        async fn set(
            &self,
            _credential_ref: &str,
            _credential: &Credential,
        ) -> Result<(), AppError> {
            Err(AppError::CredentialStoreFailed)
        }

        async fn delete(&self, _credential_ref: &str) -> Result<(), AppError> {
            Err(AppError::CredentialStoreFailed)
        }
    }

    #[derive(Default)]
    struct RecordingPipeline {
        request: Mutex<Option<DownloadStartRequest>>,
        started: Notify,
    }

    #[async_trait]
    impl DownloadPipeline for RecordingPipeline {
        async fn run(
            &self,
            request: DownloadStartRequest,
            context: TaskContext,
        ) -> Result<CompletedDownload, AppError> {
            *self.request.lock().await = Some(request);
            self.started.notify_waiters();
            context.cancelled().await;
            Err(AppError::TaskCancelled)
        }
    }

    struct FakeSystem {
        selection: Option<String>,
        opened: AtomicUsize,
        opened_path: StdMutex<Option<PathBuf>>,
    }

    #[async_trait]
    impl SystemActions for FakeSystem {
        async fn select_directory(
            &self,
            _initial_directory: Option<String>,
        ) -> Result<Option<String>, AppError> {
            Ok(self.selection.clone())
        }

        fn open_directory(&self, path: &Path) -> Result<(), AppError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            *self.opened_path.lock().unwrap() = Some(path.to_owned());
            Ok(())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        state: AppState,
        pipeline: Arc<RecordingPipeline>,
        system: Arc<FakeSystem>,
    }

    impl Fixture {
        async fn new(selection: Option<String>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config = ConfigStore::open(
                dir.path().join("config.json"),
                AppSettings {
                    default_output_dir: "downloads".into(),
                    default_arch: "amd64".into(),
                    default_mirror: "registry.test".into(),
                    download_workers: 4,
                    last_package_name: Some("keep/me".into()),
                },
                Arc::new(MemoryCredentialStore::default()),
            )
            .await
            .unwrap();
            let pipeline = Arc::new(RecordingPipeline::default());
            let tasks = TaskManager::new(
                pipeline.clone(),
                Arc::new(SilentEvents),
                Arc::new(config.clone()),
            );
            let system = Arc::new(FakeSystem {
                selection,
                opened: AtomicUsize::new(0),
                opened_path: StdMutex::new(None),
            });
            let downloads = Arc::new(DownloadCoordinator::new(config.clone(), tasks));
            let state = AppState::new(config, downloads, system.clone());
            Self {
                _dir: dir,
                state,
                pipeline,
                system,
            }
        }
    }

    #[test]
    fn exposes_every_command_implementation_boundary() {
        let _ = (
            std::any::type_name::<AppState>(),
            download_start_impl,
            download_cancel_impl,
            download_status_impl,
            settings_get_impl,
            settings_update_impl,
            mirrors_list_impl,
            mirror_add_impl,
            mirror_update_impl,
            mirror_delete_impl,
            mirror_set_default_impl,
            history_list_impl,
            history_delete_impl,
            history_clear_impl,
            dialog_select_directory_impl,
            path_open_parent_impl,
        );
    }

    #[test]
    fn capability_does_not_expose_raw_dialog_opener_or_shell_commands() {
        let capability: Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let permissions = capability["permissions"].as_array().unwrap();
        let serialized = serde_json::to_string(permissions).unwrap();
        assert!(!serialized.contains("dialog:"));
        assert!(!serialized.contains("opener:"));
        assert!(!serialized.contains("shell:"));
    }

    #[tokio::test]
    async fn state_startup_check_does_not_resolve_stored_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = ConfigStore::open(
            path.clone(),
            AppSettings {
                default_output_dir: "downloads".into(),
                default_arch: "amd64".into(),
                default_mirror: "private.test".into(),
                download_workers: 4,
                last_package_name: None,
            },
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        original
            .add_mirror(MirrorInput {
                name: "Private".into(),
                registry: "private.test".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("secret".into()),
            })
            .await
            .unwrap();
        drop(original);

        let config = ConfigStore::open(
            path,
            AppSettings {
                default_output_dir: String::new(),
                default_arch: String::new(),
                default_mirror: String::new(),
                download_workers: 1,
                last_package_name: None,
            },
            Arc::new(UnavailableCredentials),
        )
        .await
        .unwrap();
        assert!(config.has_mirrors().await);
        let tasks = TaskManager::new(
            Arc::new(RecordingPipeline::default()),
            Arc::new(SilentEvents),
            Arc::new(config.clone()),
        );
        let downloads = Arc::new(DownloadCoordinator::new(config.clone(), tasks));
        let system = Arc::new(FakeSystem {
            selection: None,
            opened: AtomicUsize::new(0),
            opened_path: StdMutex::new(None),
        });
        let state = AppState::new(config, downloads, system);
        assert_eq!(
            settings_get_impl(&state).await.unwrap().default_arch,
            "amd64"
        );
    }

    #[tokio::test]
    async fn settings_update_is_partial() {
        let fixture = Fixture::new(None).await;
        let updated = settings_update_impl(
            &fixture.state,
            SettingsPatch {
                download_workers: Some(8),
                ..SettingsPatch::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.download_workers, 8);
        assert_eq!(updated.default_output_dir, "downloads");
        assert_eq!(updated.last_package_name.as_deref(), Some("keep/me"));
        assert_eq!(settings_get_impl(&fixture.state).await.unwrap(), updated);
    }

    #[tokio::test]
    async fn mirror_commands_preserve_credentials_and_default_invariants() {
        let fixture = Fixture::new(None).await;
        let first = mirror_add_impl(
            &fixture.state,
            MirrorInput {
                name: "Private".into(),
                registry: "private.test".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("secret".into()),
            },
        )
        .await
        .unwrap();
        let second = mirror_add_impl(
            &fixture.state,
            MirrorInput {
                name: "Public".into(),
                registry: "public.test".into(),
                is_default: false,
                username: None,
                password: None,
            },
        )
        .await
        .unwrap();
        let listed = mirrors_list_impl(&fixture.state).await.unwrap();
        assert_eq!(listed[0].username.as_deref(), Some("alice"));
        assert_eq!(listed[0].password.as_deref(), Some("secret"));

        let settings = mirror_set_default_impl(&fixture.state, second.id.clone())
            .await
            .unwrap();
        assert_eq!(settings.default_mirror, "public.test");
        let updated = mirror_update_impl(
            &fixture.state,
            second.id.clone(),
            MirrorPatch {
                name: Some("Public mirror".into()),
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..MirrorPatch::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "Public mirror");
        assert_eq!(updated.password.as_deref(), Some("new-secret"));
        assert!(
            mirror_delete_impl(&fixture.state, first.id)
                .await
                .unwrap()
                .success
        );
        assert_eq!(mirrors_list_impl(&fixture.state).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn download_uses_stored_mirror_credentials_and_rejects_second_start() {
        let fixture = Fixture::new(None).await;
        let mirror = mirror_add_impl(
            &fixture.state,
            MirrorInput {
                name: "Private".into(),
                registry: "private.test".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("stored-secret".into()),
            },
        )
        .await
        .unwrap();
        let request = DownloadStartRequest {
            package_name: "library/alpine".into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: mirror.id,
            output_dir: fixture._dir.path().to_string_lossy().into_owned(),
            username: None,
            password: None,
        };
        let snapshot = download_start_impl(&fixture.state, request.clone())
            .await
            .unwrap();
        assert_eq!(snapshot.status, DownloadStatus::Preparing);
        let error = download_start_impl(&fixture.state, request)
            .await
            .unwrap_err();
        assert_eq!(error.code, "TASK_ALREADY_RUNNING");
        assert!(!error.retryable);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.pipeline.started.notified(),
        )
        .await
        .ok();
        let captured = fixture.pipeline.request.lock().await.clone().unwrap();
        assert_eq!(captured.mirror, "private.test");
        assert_eq!(captured.username.as_deref(), Some("alice"));
        assert_eq!(captured.password.as_deref(), Some("stored-secret"));
        let cancelled = download_cancel_impl(&fixture.state).await.unwrap();
        assert_eq!(cancelled.task_id, snapshot.task_id);
        assert_eq!(
            download_status_impl(&fixture.state).await.unwrap().task_id,
            snapshot.task_id
        );
    }

    #[tokio::test]
    async fn history_delete_and_clear_are_persisted() {
        let fixture = Fixture::new(None).await;
        for id in ["one", "two"] {
            fixture.state.config.add_history(history(id)).await.unwrap();
        }
        assert_eq!(history_list_impl(&fixture.state).await.unwrap().len(), 2);
        assert!(
            history_delete_impl(&fixture.state, "one".into())
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            history_list_impl(&fixture.state).await.unwrap()[0].id,
            "two"
        );
        assert!(history_clear_impl(&fixture.state).await.unwrap().success);
        assert!(history_list_impl(&fixture.state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_dialog_returns_null_and_nonexistent_path_is_rejected() {
        let fixture = Fixture::new(None).await;
        assert_eq!(
            dialog_select_directory_impl(&fixture.state, Some("C:/initial".into()))
                .await
                .unwrap(),
            None
        );
        let missing = fixture._dir.path().join("missing.tar");
        let error = path_open_parent_impl(&fixture.state, missing.to_string_lossy().into_owned())
            .await
            .unwrap_err();
        assert_eq!(error.code, "OUTPUT_NOT_WRITABLE");
        assert_eq!(fixture.system.opened.load(Ordering::SeqCst), 0);

        let directory_error = path_open_parent_impl(
            &fixture.state,
            fixture._dir.path().to_string_lossy().into_owned(),
        )
        .await
        .unwrap_err();
        assert_eq!(directory_error.code, "OUTPUT_NOT_WRITABLE");

        let nested = fixture._dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("archive.tar");
        std::fs::write(&file, b"tar").unwrap();
        assert!(
            path_open_parent_impl(&fixture.state, file.to_string_lossy().into_owned())
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            fixture.system.opened_path.lock().unwrap().as_deref(),
            Some(nested.canonicalize().unwrap().as_path())
        );
    }

    fn history(id: &str) -> HistoryItem {
        HistoryItem {
            id: id.into(),
            package_name: "alpine".into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: "registry.test".into(),
            full_image: "registry.test/library/alpine:latest".into(),
            output_path: None,
            size: None,
            status: DownloadStatus::Completed,
            downloaded_at: None,
            error: None,
        }
    }
}
