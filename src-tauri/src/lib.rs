pub mod archive;
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

use crate::commands::{AppState, TauriSystemActions};
use crate::credentials::WindowsCredentialStore;
use crate::domain::{AppSettings, MirrorInput};
use crate::download_pipeline::{DownloadCoordinator, ProductionDownloadPipeline};
use crate::events::EventSink;
use crate::storage::ConfigStore;
use crate::task_manager::TaskManager;
use serde_json::Value;
use std::io;
use std::sync::Arc;
use tauri::{Emitter, Manager};

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
                let config = ConfigStore::open(
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
                let tasks = TaskManager::new(pipeline, events, Arc::new(config.clone()));
                let downloads = Arc::new(DownloadCoordinator::new(config.clone(), tasks));
                Ok::<_, crate::error::AppError>(AppState::new(
                    config,
                    downloads,
                    Arc::new(TauriSystemActions::new(app_handle)),
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
    use super::{DOCKER_HUB_REGISTRY, ensure_default_mirror};
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
}
