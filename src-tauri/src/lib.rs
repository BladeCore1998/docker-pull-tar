pub mod archive;
pub mod batch_manager;
pub mod blob;
pub mod commands;
pub mod credentials;
pub mod domain;
pub mod download_pipeline;
pub mod error;
pub mod events;
pub mod image_ref;
pub mod registry;
pub mod storage;
pub mod task_manager;

use crate::batch_manager::BatchManager;
use crate::commands::{AppState, TauriSystemActions};
use crate::credentials::{CredentialStore, WindowsCredentialStore};
use crate::domain::{AppSettings, MirrorInput};
use crate::download_pipeline::{DownloadCoordinator, ProductionDownloadPipeline};
use crate::error::AppError;
use crate::events::EventSink;
use crate::storage::ConfigStore;
use crate::task_manager::TaskManager;
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{Emitter, Manager};
use uuid::Uuid;

const DOCKER_HUB_REGISTRY: &str = "registry-1.docker.io";

async fn ensure_default_mirror(config: &ConfigStore) -> Result<(), crate::error::AppError> {
    if !config.has_mirrors().await {
        config
            .add_mirror(MirrorInput {
                name: "Docker Hub".into(),
                registry: DOCKER_HUB_REGISTRY.into(),
                is_default: true,
                username: None,
                password: None,
            })
            .await?;
    }
    Ok(())
}

async fn quarantine_corrupt_config(path: &Path) -> Result<(), AppError> {
    let candidates = [
        path.to_path_buf(),
        path.with_file_name(format!(
            "{}.recovery",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("config.json")
        )),
    ];
    let suffix = format!("startup-corrupt-{}", Uuid::new_v4());

    for candidate in candidates {
        let metadata = match tokio::fs::metadata(&candidate).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(AppError::ConfigIoFailed),
        };
        if !metadata.is_file() {
            return Err(AppError::ConfigIoFailed);
        }
        let file_name = candidate
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.json");
        let backup = candidate.with_file_name(format!("{file_name}.{suffix}"));
        tokio::fs::rename(&candidate, backup)
            .await
            .map_err(|_| AppError::ConfigIoFailed)?;
    }
    Ok(())
}

async fn open_config_for_app(
    path: PathBuf,
    default_settings: AppSettings,
    credentials: Arc<dyn CredentialStore>,
) -> Result<ConfigStore, AppError> {
    match ConfigStore::open(path.clone(), default_settings.clone(), credentials.clone()).await {
        Ok(config) => Ok(config),
        Err(AppError::ConfigCorrupted) => {
            quarantine_corrupt_config(&path).await?;
            eprintln!("configuration was quarantined and reset after a corruption check");
            ConfigStore::open(path, default_settings, credentials).await
        }
        Err(error) => Err(error),
    }
}

struct TauriEventSink(tauri::AppHandle);

impl EventSink for TauriEventSink {
    fn emit(&self, event: &'static str, payload: Value) {
        let _ = self.0.emit(event, payload);
    }
}

pub fn app_name() -> &'static str {
    "Docker Pull Tar"
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let app_handle = app.handle().clone();
            let config_dir = app
                .path()
                .config_dir()
                .map_err(|error| io::Error::other(error.to_string()))?
                .join("docker-pull-tar");
            let default_output_dir = app
                .path()
                .download_dir()
                .unwrap_or_else(|_| config_dir.clone())
                .to_string_lossy()
                .into_owned();
            let state = tauri::async_runtime::block_on(async move {
                let config = open_config_for_app(
                    config_dir.join("config.json"),
                    AppSettings {
                        default_output_dir,
                        default_arch: "amd64".into(),
                        default_mirror: DOCKER_HUB_REGISTRY.into(),
                        download_workers: 4,
                        last_package_name: None,
                    },
                    Arc::new(WindowsCredentialStore::new("docker-pull-tar")),
                )
                .await?;
                ensure_default_mirror(&config).await?;
                let pipeline = Arc::new(ProductionDownloadPipeline::from_config(config.clone())?);
                let events = Arc::new(TauriEventSink(app_handle.clone()));
                let tasks = TaskManager::new(pipeline.clone(), events, Arc::new(config.clone()));
                let downloads = Arc::new(DownloadCoordinator::new(config.clone(), tasks));
                let batch_events = Arc::new(TauriEventSink(app_handle.clone()));
                let batches = Arc::new(BatchManager::new(
                    config.clone(),
                    pipeline,
                    batch_events,
                    Arc::new(config.clone()),
                ));
                Ok::<_, crate::error::AppError>(AppState::with_batches(
                    config,
                    downloads,
                    Arc::new(TauriSystemActions::new(app_handle)),
                    batches,
                ))
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::download_start,
            commands::download_cancel,
            commands::download_status,
            commands::batch_start,
            commands::batch_status,
            commands::batch_cancel,
            commands::batch_task_cancel,
            commands::batch_task_retry,
            commands::settings_get,
            commands::settings_update,
            commands::mirrors_list,
            commands::mirror_add,
            commands::mirror_update,
            commands::mirror_delete,
            commands::mirror_set_default,
            commands::history_list,
            commands::history_delete,
            commands::history_clear,
            commands::dialog_select_directory,
            commands::path_open_parent,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Docker Pull Tar");
}

#[cfg(test)]
mod tests {
    use super::{DOCKER_HUB_REGISTRY, ensure_default_mirror, open_config_for_app};
    use crate::credentials::MemoryCredentialStore;
    use crate::domain::AppSettings;
    use crate::storage::ConfigStore;
    use std::sync::Arc;

    #[test]
    fn exposes_stable_app_name() {
        assert_eq!(super::app_name(), "Docker Pull Tar");
    }

    #[tokio::test]
    async fn empty_config_gets_docker_hub_as_default_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigStore::open(
            dir.path().join("config.json"),
            AppSettings {
                default_output_dir: dir.path().to_string_lossy().into_owned(),
                default_arch: "amd64".into(),
                default_mirror: DOCKER_HUB_REGISTRY.into(),
                download_workers: 4,
                last_package_name: None,
            },
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap();

        ensure_default_mirror(&config).await.unwrap();
        let mirrors = config.mirrors().await.unwrap();
        assert_eq!(mirrors.len(), 1);
        assert_eq!(mirrors[0].name, "Docker Hub");
        assert_eq!(mirrors[0].registry, DOCKER_HUB_REGISTRY);
        assert!(mirrors[0].is_default);
        assert_eq!(config.settings().await.default_mirror, DOCKER_HUB_REGISTRY);
    }

    #[tokio::test]
    async fn corrupt_startup_config_is_quarantined_and_replaced_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::write(
            &path,
            br#"{
                "version": "3.0",
                "revision": 1,
                "transactionId": "legacy",
                "settings": {
                    "defaultOutputDir": "C:\\downloads",
                    "defaultArch": "amd64",
                    "defaultMirror": "registry.example",
                    "downloadWorkers": 4,
                    "lastPackageName": null
                },
                "mirrors": [{
                    "id": "legacy",
                    "name": "Legacy",
                    "registry": "registry.example",
                    "isDefault": true,
                    "username": "user",
                    "password": "plaintext"
                }]
            }"#,
        )
        .await
        .unwrap();

        let config = open_config_for_app(
            path.clone(),
            AppSettings {
                default_output_dir: dir.path().to_string_lossy().into_owned(),
                default_arch: "amd64".into(),
                default_mirror: DOCKER_HUB_REGISTRY.into(),
                download_workers: 4,
                last_package_name: None,
            },
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        ensure_default_mirror(&config).await.unwrap();

        assert!(path.is_file());
        assert_eq!(
            config.mirrors().await.unwrap()[0].registry,
            DOCKER_HUB_REGISTRY
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .contains("startup-corrupt-")
                })
                .count(),
            1
        );
    }
}
