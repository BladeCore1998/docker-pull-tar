use async_trait::async_trait;
use docker_pull_tar_lib::credentials::{Credential, CredentialStore, MemoryCredentialStore};
use docker_pull_tar_lib::domain::{
    AppSettings, DownloadStatus, HistoryItem, MirrorInput, MirrorPatch, SettingsPatch,
};
use docker_pull_tar_lib::error::AppError;
use docker_pull_tar_lib::storage::{
    ConfigStore, StorageMaintenanceWarning, history_redownload_registry,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

#[derive(Default)]
struct FailingCredentialStore {
    values: Mutex<HashMap<String, Credential>>,
    set_calls: Mutex<usize>,
    fail_set_at: Mutex<Option<usize>>,
    fail_set_without_mutation_at: Mutex<Option<usize>>,
    fail_next_get: Mutex<bool>,
    block_config_recovery_after_set_at: Mutex<Option<(usize, PathBuf)>>,
    fail_get_after_delete_at: Mutex<Option<usize>>,
    delete_calls: Mutex<usize>,
    fail_delete_at: Mutex<Option<usize>>,
    fail_delete_without_mutation_at: Mutex<Option<usize>>,
    block_config_restore_at: Mutex<Option<(usize, PathBuf)>>,
    block_config_recovery_at: Mutex<Option<(usize, PathBuf)>>,
}

impl FailingCredentialStore {
    async fn fail_next_get(&self) {
        *self.fail_next_get.lock().await = true;
    }
    async fn fail_set_at(&self, call: usize) {
        *self.fail_set_at.lock().await = Some(call);
    }
    async fn fail_set_without_mutation_at(&self, call: usize) {
        *self.fail_set_without_mutation_at.lock().await = Some(call);
    }
    async fn block_config_recovery_after_set_at(&self, call: usize, path: PathBuf) {
        *self.block_config_recovery_after_set_at.lock().await = Some((call, path));
    }
    async fn fail_get_after_delete_at(&self, call: usize) {
        *self.fail_get_after_delete_at.lock().await = Some(call);
    }
    async fn values(&self) -> HashMap<String, Credential> {
        self.values.lock().await.clone()
    }
    async fn insert(&self, key: &str, value: Credential) {
        self.values.lock().await.insert(key.into(), value);
    }
    async fn fail_delete_at(&self, call: usize) {
        *self.fail_delete_at.lock().await = Some(call);
    }
    async fn fail_delete_without_mutation_at(&self, call: usize) {
        *self.fail_delete_without_mutation_at.lock().await = Some(call);
    }
    async fn fail_delete_and_block_config_restore(&self, call: usize, path: PathBuf) {
        *self.fail_delete_at.lock().await = Some(call);
        *self.block_config_restore_at.lock().await = Some((call, path));
    }
    async fn block_config_recovery_at(&self, call: usize, path: PathBuf) {
        *self.block_config_recovery_at.lock().await = Some((call, path));
    }
}

#[async_trait]
impl CredentialStore for FailingCredentialStore {
    async fn get(&self, key: &str) -> Result<Option<Credential>, AppError> {
        let mut fail = self.fail_next_get.lock().await;
        if *fail {
            *fail = false;
            return Err(AppError::CredentialStoreFailed);
        }
        Ok(self.values.lock().await.get(key).cloned())
    }
    async fn set(&self, key: &str, value: &Credential) -> Result<(), AppError> {
        let mut calls = self.set_calls.lock().await;
        *calls += 1;
        if *self.fail_set_without_mutation_at.lock().await == Some(*calls) {
            return Err(AppError::CredentialStoreFailed);
        }
        self.values.lock().await.insert(key.into(), value.clone());
        if let Some((call, path)) = self
            .block_config_recovery_after_set_at
            .lock()
            .await
            .as_ref()
            && *call == *calls
        {
            let recovery = path.with_file_name(format!(
                "{}.recovery",
                path.file_name().unwrap().to_string_lossy()
            ));
            match tokio::fs::remove_file(&recovery).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(AppError::ConfigIoFailed),
            }
            tokio::fs::create_dir(recovery)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
        }
        if *self.fail_set_at.lock().await == Some(*calls) {
            return Err(AppError::CredentialStoreFailed);
        }
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), AppError> {
        let mut calls = self.delete_calls.lock().await;
        *calls += 1;
        if *self.fail_delete_without_mutation_at.lock().await == Some(*calls) {
            return Err(AppError::CredentialStoreFailed);
        }
        self.values.lock().await.remove(key);
        if *self.fail_get_after_delete_at.lock().await == Some(*calls) {
            *self.fail_next_get.lock().await = true;
        }
        if let Some((call, path)) = self.block_config_restore_at.lock().await.as_ref()
            && *call == *calls
        {
            tokio::fs::remove_file(path)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
            tokio::fs::create_dir(path)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
        }
        if let Some((call, path)) = self.block_config_recovery_at.lock().await.as_ref()
            && *call == *calls
        {
            let recovery = path.with_file_name(format!(
                "{}.recovery",
                path.file_name().unwrap().to_string_lossy()
            ));
            match tokio::fs::remove_file(&recovery).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(AppError::ConfigIoFailed),
            }
            tokio::fs::create_dir(recovery)
                .await
                .map_err(|_| AppError::ConfigIoFailed)?;
        }
        if *self.fail_delete_at.lock().await == Some(*calls) {
            *self.fail_delete_at.lock().await = None;
            return Err(AppError::CredentialStoreFailed);
        }
        Ok(())
    }
}

fn settings() -> AppSettings {
    AppSettings {
        default_output_dir: "downloads".into(),
        default_arch: "amd64".into(),
        default_mirror: "docker.io".into(),
        download_workers: 4,
        last_package_name: None,
    }
}

async fn store(dir: &TempDir) -> ConfigStore {
    ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap()
}

async fn persisted_config(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&tokio::fs::read_to_string(path).await.unwrap()).unwrap()
}

fn v3_snapshot(revision: u64, transaction_id: &str, arch: &str) -> serde_json::Value {
    json!({
        "version":"3.0",
        "revision":revision,
        "transactionId":transaction_id,
        "settings": {
            "defaultOutputDir":"downloads",
            "defaultArch":arch,
            "defaultMirror":"docker.io",
            "downloadWorkers":4,
            "lastPackageName":null
        },
        "mirrors":[],
        "history":[],
        "pendingCredentialCleanup":[]
    })
}

fn mirror(name: &str, registry: &str, is_default: bool) -> MirrorInput {
    MirrorInput {
        name: name.into(),
        registry: registry.into(),
        is_default,
        username: Some("alice".into()),
        password: Some("secret".into()),
    }
}

fn recovery_path(path: &std::path::Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.recovery",
        path.file_name().unwrap().to_string_lossy()
    ))
}

fn stored_credential_ref(config: &serde_json::Value, mirror_id: &str) -> String {
    config["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mirror| mirror["id"] == mirror_id)
        .unwrap()["credentialRef"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn partial_settings_update_preserves_omitted_fields() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    store
        .update_settings(SettingsPatch {
            download_workers: Some(8),
            ..Default::default()
        })
        .await
        .unwrap();
    let actual = store.settings().await;
    assert_eq!(actual.default_output_dir, "downloads");
    assert_eq!(actual.download_workers, 8);
}

#[tokio::test]
async fn first_open_persists_revisioned_v3_snapshot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");

    let opened = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();

    let persisted = persisted_config(&path).await;
    assert_eq!(persisted["version"], "3.0");
    assert_eq!(persisted["revision"], 1);
    assert!(!persisted["transactionId"].as_str().unwrap().is_empty());
    drop(opened);

    ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();
    assert_eq!(persisted_config(&path).await["revision"], 1);
}

#[tokio::test]
async fn concurrent_mutations_serialize_unique_monotonic_revisions_without_loss() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let store = Arc::new(store(&dir).await);
    let settings_store = Arc::clone(&store);
    let mirror_store = Arc::clone(&store);
    let history_store = Arc::clone(&store);

    let (settings_result, mirror_result, history_result) = tokio::join!(
        settings_store.update_settings(SettingsPatch {
            default_arch: Some("arm64".into()),
            ..Default::default()
        }),
        mirror_store.add_mirror(mirror("Docker", "docker.io", true)),
        history_store.add_history(HistoryItem {
            id: "concurrent-history".into(),
            package_name: "busybox".into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: "docker.io".into(),
            full_image: "busybox:latest".into(),
            output_path: None,
            size: None,
            status: DownloadStatus::Completed,
            downloaded_at: None,
            error: None,
        })
    );
    settings_result.unwrap();
    mirror_result.unwrap();
    history_result.unwrap();

    let persisted = persisted_config(&path).await;
    assert_eq!(persisted["revision"], 5);
    assert_eq!(store.settings().await.default_arch, "arm64");
    assert_eq!(store.mirrors().await.unwrap().len(), 1);
    assert_eq!(store.history().await[0].id, "concurrent-history");
    let transaction_id = persisted["transactionId"].as_str().unwrap().to_owned();
    assert!(!transaction_id.is_empty());

    drop(store);
    let reopened = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();
    assert_eq!(persisted_config(&path).await["revision"], 5);
    assert_eq!(reopened.settings().await.default_arch, "arm64");
    assert_eq!(reopened.history().await[0].id, "concurrent-history");
}

#[tokio::test]
async fn credential_workflow_allocates_prepare_and_activation_revisions() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let store = store(&dir).await;
    let mut observed = Vec::new();

    observed.push(persisted_config(&path).await);
    store
        .update_settings(SettingsPatch {
            default_arch: Some("arm64".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    observed.push(persisted_config(&path).await);
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    observed.push(persisted_config(&path).await);
    store
        .add_history(HistoryItem {
            id: "transaction-observation".into(),
            package_name: "busybox".into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: "docker.io".into(),
            full_image: "busybox:latest".into(),
            output_path: None,
            size: None,
            status: DownloadStatus::Completed,
            downloaded_at: None,
            error: None,
        })
        .await
        .unwrap();
    observed.push(persisted_config(&path).await);

    let revisions: Vec<_> = observed
        .iter()
        .map(|snapshot| snapshot["revision"].as_u64().unwrap())
        .collect();
    assert_eq!(revisions, vec![1, 2, 4, 5]);
    let transaction_ids: std::collections::HashSet<_> = observed
        .iter()
        .map(|snapshot| snapshot["transactionId"].as_str().unwrap())
        .collect();
    assert_eq!(transaction_ids.len(), observed.len());
    assert!(transaction_ids.iter().all(|id| !id.trim().is_empty()));
}

#[tokio::test]
async fn newer_main_removes_stale_recovery_before_open_returns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    tokio::fs::write(&path, v3_snapshot(8, "main-newer", "arm64").to_string())
        .await
        .unwrap();
    tokio::fs::write(
        &recovery,
        v3_snapshot(7, "recovery-stale", "amd64").to_string(),
    )
    .await
    .unwrap();

    let opened = ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();

    assert_eq!(opened.settings().await.default_arch, "arm64");
    assert!(!recovery.exists());
}

#[tokio::test]
async fn equal_exact_recovery_is_removed_before_open_returns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    let snapshot = v3_snapshot(8, "same-transaction", "arm64").to_string();
    tokio::fs::write(&path, &snapshot).await.unwrap();
    tokio::fs::write(&recovery, snapshot).await.unwrap();

    let opened = ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();

    assert_eq!(opened.settings().await.default_arch, "arm64");
    assert!(!recovery.exists());
    assert!(opened.take_maintenance_warnings().await.is_empty());
}

#[tokio::test]
async fn mirror_crud_keeps_exactly_one_default_and_hides_password_from_json() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let first = store
        .add_mirror(mirror("Docker", " HTTPS://Docker.IO/ ", false))
        .await
        .unwrap();
    let second = store
        .add_mirror(mirror("GHCR", "ghcr.io", true))
        .await
        .unwrap();
    assert_eq!(
        store
            .mirrors()
            .await
            .unwrap()
            .iter()
            .filter(|m| m.is_default)
            .count(),
        1
    );
    assert!(store.mirror(&second.id).await.unwrap().password.is_some());
    let raw = tokio::fs::read_to_string(dir.path().join("config.json"))
        .await
        .unwrap();
    assert!(!raw.contains("secret"));
    assert!(!raw.contains("password"));
    assert!(raw.contains("credentialRef"));
    assert!(store.delete_mirror(&first.id).await.is_ok());
}

#[tokio::test]
async fn concurrent_settings_and_mirror_updates_do_not_lose_data() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(store(&dir).await);
    let a = Arc::clone(&store);
    let b = Arc::clone(&store);
    let (settings_result, mirror_result) = tokio::join!(
        a.update_settings(SettingsPatch {
            default_arch: Some("arm64".into()),
            ..Default::default()
        }),
        b.add_mirror(mirror("Docker", "docker.io", true))
    );
    settings_result.unwrap();
    mirror_result.unwrap();
    assert_eq!(store.settings().await.default_arch, "arm64");
    assert_eq!(store.mirrors().await.unwrap().len(), 1);
}

#[tokio::test]
async fn migrates_v1_plaintext_credentials_and_rewrites_without_password() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::write(
        &path,
        json!({
            "version": "1.0", "settings": settings(), "mirrors": [{
                "id": "old", "name": "Old", "registry": "docker.io", "isDefault": true,
                "username": "legacy", "password": "plain"
            }], "history": []
        })
        .to_string(),
    )
    .await
    .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    assert_eq!(
        store.mirror("old").await.unwrap().password.as_deref(),
        Some("plain")
    );
    let raw = tokio::fs::read_to_string(path).await.unwrap();
    assert!(!raw.contains("plain"));
    let persisted: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(persisted["version"], "3.0");
    assert_eq!(persisted["revision"], 1);
    assert!(!persisted["transactionId"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn migrates_v2_recovery_only_to_revisioned_v3_and_reopens() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    tokio::fs::write(
        &recovery,
        json!({
            "version":"2.0",
            "settings": {
                "defaultOutputDir":"downloads", "defaultArch":"arm64",
                "defaultMirror":"docker.io", "downloadWorkers":4,
                "lastPackageName":null
            },
            "mirrors":[],
            "history":[]
        })
        .to_string(),
    )
    .await
    .unwrap();

    let opened = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();
    assert_eq!(opened.settings().await.default_arch, "arm64");
    let persisted = persisted_config(&path).await;
    assert_eq!(persisted["version"], "3.0");
    assert_eq!(persisted["revision"], 1);
    drop(opened);

    let reopened = ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();
    assert_eq!(reopened.settings().await.default_arch, "arm64");
}

#[tokio::test]
async fn legacy_main_with_committed_v3_recovery_reopens_the_recovery_revision() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    tokio::fs::write(
        &path,
        json!({
            "version":"2.0", "settings":settings(), "mirrors":[], "history":[],
            "pendingCredentialCleanup":[]
        })
        .to_string(),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &recovery,
        v3_snapshot(1, "migration-commit", "arm64").to_string(),
    )
    .await
    .unwrap();

    let opened = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();
    assert_eq!(opened.settings().await.default_arch, "arm64");
    drop(opened);

    let reopened = ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();
    assert_eq!(reopened.settings().await.default_arch, "arm64");
}

#[tokio::test]
async fn conflicting_v2_candidates_fail_and_preserve_both_files() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    let main_bytes = json!({
        "version":"2.0", "settings":settings(), "mirrors":[], "history":[],
        "pendingCredentialCleanup":[]
    })
    .to_string();
    let mut recovery_settings = settings();
    recovery_settings.default_arch = "arm64".into();
    let recovery_bytes = json!({
        "version":"2.0", "settings":recovery_settings, "mirrors":[], "history":[],
        "pendingCredentialCleanup":[]
    })
    .to_string();
    tokio::fs::write(&path, &main_bytes).await.unwrap();
    tokio::fs::write(&recovery, &recovery_bytes).await.unwrap();

    let error = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, AppError::ConfigCorrupted));
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), main_bytes);
    assert_eq!(
        tokio::fs::read_to_string(recovery).await.unwrap(),
        recovery_bytes
    );
}

#[tokio::test]
async fn corrupt_recovery_fails_and_preserves_main_and_recovery_evidence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let recovery = dir.path().join("config.json.recovery");
    let main_bytes = v3_snapshot(4, "known-main", "amd64").to_string();
    let recovery_bytes = "{not-valid-json";
    tokio::fs::write(&path, &main_bytes).await.unwrap();
    tokio::fs::write(&recovery, recovery_bytes).await.unwrap();

    let error = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, AppError::ConfigCorrupted));
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), main_bytes);
    assert_eq!(
        tokio::fs::read_to_string(recovery).await.unwrap(),
        recovery_bytes
    );
}

#[tokio::test]
async fn revision_overflow_leaves_disk_and_memory_unchanged() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::write(
        &path,
        json!({
            "version":"3.0", "revision":u64::MAX, "transactionId":"max-revision",
            "settings":settings(), "mirrors":[], "history":[],
            "pendingCredentialCleanup":[]
        })
        .to_string(),
    )
    .await
    .unwrap();
    let before = tokio::fs::read(&path).await.unwrap();
    let store = ConfigStore::open(
        path.clone(),
        settings(),
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();

    let error = store
        .update_settings(SettingsPatch {
            download_workers: Some(9),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::ConfigCorrupted));
    assert_eq!(store.settings().await.download_workers, 4);
    assert_eq!(tokio::fs::read(path).await.unwrap(), before);
}

#[tokio::test]
async fn corrupt_config_is_backed_up_before_defaults_are_created() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::write(&path, "not json").await.unwrap();
    ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();
    let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
    let mut backed_up = false;
    while let Some(entry) = entries.next_entry().await.unwrap() {
        backed_up |= entry
            .file_name()
            .to_string_lossy()
            .contains("config.json.corrupt-");
    }
    assert!(backed_up);
}

#[tokio::test]
async fn mirror_mutations_enforce_default_and_existence_rules() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    assert!(store.delete_mirror("missing").await.is_err());
    let only = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    assert!(store.delete_mirror(&only.id).await.is_err());
    assert!(
        store
            .update_mirror(
                &only.id,
                MirrorPatch {
                    is_default: Some(false),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(store.set_default_mirror("missing").await.is_err());
    assert!(
        store
            .add_mirror(mirror("Duplicate", " docker.io/ ", false))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn history_is_capped_and_redownload_falls_back_to_current_default_registry() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    for index in 0..105 {
        store
            .add_history(HistoryItem {
                id: index.to_string(),
                package_name: "nginx".into(),
                tag: "latest".into(),
                arch: "amd64".into(),
                mirror: "removed.example".into(),
                full_image: "nginx:latest".into(),
                output_path: None,
                size: None,
                status: DownloadStatus::Completed,
                downloaded_at: None,
                error: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(store.history().await.len(), 100);
    assert_eq!(
        history_redownload_registry(&store.history().await[0], &[], "docker.io"),
        "docker.io"
    );
}

#[tokio::test]
async fn memory_credentials_round_trip() {
    let store = MemoryCredentialStore::default();
    let value = Credential {
        username: Some("alice".into()),
        password: "secret".into(),
    };
    store.set("ref", &value).await.unwrap();
    assert_eq!(store.get("ref").await.unwrap(), Some(value));
    store.delete("ref").await.unwrap();
    assert_eq!(store.get("ref").await.unwrap(), None);
}

#[tokio::test]
async fn updating_default_registry_syncs_settings_and_invalidates_credentials() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.settings().await.default_mirror,
        "registry.example.com"
    );
    assert_eq!(updated.password, None);
    let raw = tokio::fs::read_to_string(dir.path().join("config.json"))
        .await
        .unwrap();
    assert!(!raw.contains("credentialRef"));
}

#[tokio::test]
async fn registry_change_that_removes_auth_does_not_require_a_precommit_credential_read() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &added.id);
    credentials.fail_next_get().await;

    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.registry, "registry.example.com");
    assert_eq!(updated.password, None);
    assert!(!credentials.values().await.contains_key(&old_ref));
}

#[tokio::test]
async fn failed_main_promotion_keeps_committed_registry_update() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.registry, "registry.example.com");
    assert_eq!(updated.password, None);
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::MainPromotionDeferred; 2]
    );
    assert!(tokio::fs::metadata(path).await.unwrap().is_dir());
}

#[tokio::test]
async fn migration_rolls_back_all_credentials_when_a_later_write_fails() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::write(
        &path,
        json!({
            "version": "1.0", "settings": settings(), "mirrors": [
                {"id":"one","name":"One","registry":"one.io","isDefault":true,"password":"first"},
                {"id":"two","name":"Two","registry":"two.io","isDefault":false,"password":"second"}
            ], "history": []
        })
        .to_string(),
    )
    .await
    .unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let original = Credential {
        username: Some("original".into()),
        password: "original-secret".into(),
    };
    credentials.insert("mirror:one", original.clone()).await;
    credentials.fail_set_at(2).await;
    let error = ConfigStore::open(path, settings(), credentials.clone())
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::CredentialStoreFailed));
    assert_eq!(
        credentials.values().await,
        HashMap::from([("mirror:one".into(), original)])
    );
}

#[tokio::test]
async fn multi_field_credential_failure_leaves_original_credential() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    credentials.fail_set_at(2).await;
    let error = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::CredentialStoreFailed));
    let actual = store.mirror(&added.id).await.unwrap();
    assert_eq!(actual.username.as_deref(), Some("alice"));
    assert_eq!(actual.password.as_deref(), Some("secret"));
}

#[tokio::test]
async fn opening_normalizes_default_setting_and_caps_existing_history() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let history: Vec<_> = (0..105)
        .map(|index| {
            json!({
                "id":index.to_string(),"packageName":"nginx","tag":"latest","arch":"amd64",
                "mirror":"old","fullImage":"nginx:latest","outputPath":null,"size":null,
                "status":"completed","downloadedAt":null,"error":null
            })
        })
        .collect();
    tokio::fs::write(
        &path,
        json!({
            "version":"2.0","settings":settings(),"mirrors":[
                {"id":"m","name":"Mirror","registry":" GHCR.IO/ ","isDefault":true}
            ],"history":history
        })
        .to_string(),
    )
    .await
    .unwrap();
    let store = ConfigStore::open(path, settings(), Arc::new(MemoryCredentialStore::default()))
        .await
        .unwrap();
    assert_eq!(store.settings().await.default_mirror, "ghcr.io");
    assert_eq!(store.history().await.len(), 100);
}

#[tokio::test]
async fn registry_validation_errors_have_distinct_stable_codes() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let empty = store
        .add_mirror(mirror("Empty", " / ", false))
        .await
        .unwrap_err();
    assert_eq!(empty.code(), "INVALID_MIRROR");
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let duplicate = store
        .add_mirror(mirror("Duplicate", "docker.io/", false))
        .await
        .unwrap_err();
    assert_eq!(duplicate.code(), "MIRROR_CONFLICT");
}

#[tokio::test]
async fn settings_default_mirror_selects_exactly_one_existing_registry() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    store
        .add_mirror(mirror("GHCR", "ghcr.io", false))
        .await
        .unwrap();
    store
        .update_settings(SettingsPatch {
            default_mirror: Some(" HTTPS://GHCR.IO/ ".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mirrors = store.mirrors().await.unwrap();
    assert_eq!(store.settings().await.default_mirror, "ghcr.io");
    assert_eq!(mirrors.iter().filter(|mirror| mirror.is_default).count(), 1);
    assert!(
        mirrors
            .iter()
            .find(|mirror| mirror.registry == "ghcr.io")
            .unwrap()
            .is_default
    );
}

#[tokio::test]
async fn settings_rejects_missing_default_without_persisting_divergence() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let before = tokio::fs::read_to_string(dir.path().join("config.json"))
        .await
        .unwrap();
    let error = store
        .update_settings(SettingsPatch {
            default_mirror: Some("missing.example".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::MirrorNotFound));
    assert_eq!(store.settings().await.default_mirror, "docker.io");
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("config.json"))
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn full_edit_with_equivalent_registry_preserves_and_updates_credentials() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                name: Some("Docker Hub".into()),
                registry: Some(" HTTPS://DOCKER.IO/ ".into()),
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.registry, "docker.io");
    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    let raw = tokio::fs::read_to_string(dir.path().join("config.json"))
        .await
        .unwrap();
    assert!(raw.contains("credentialRef"));
    assert!(!raw.contains("new-secret"));
}

#[tokio::test]
async fn equivalent_registry_without_credential_patch_preserves_secret() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir).await;
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                name: Some("Docker Hub".into()),
                registry: Some("docker.io/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.password.as_deref(), Some("secret"));
    let raw = tokio::fs::read_to_string(dir.path().join("config.json"))
        .await
        .unwrap();
    assert!(raw.contains("credentialRef"));
}

#[tokio::test]
async fn registry_change_with_credentials_uses_new_credential_reference() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let before_keys: Vec<_> = credentials.values().await.into_keys().collect();
    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let after = credentials.values().await;
    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert_eq!(after.len(), 1);
    assert!(!after.contains_key(&before_keys[0]));
}

#[tokio::test]
async fn credential_set_error_after_mutation_keeps_old_active_and_new_pending() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let before = credentials.values().await;
    credentials.fail_set_at(2).await;
    let error = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                registry: Some("registry.example.com".into()),
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::CredentialStoreFailed));
    let prepared = persisted_config(&dir.path().join("config.json")).await;
    let old_ref = stored_credential_ref(&prepared, &added.id);
    let new_ref = prepared["pendingCredentialCleanup"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(new_ref, old_ref);
    let values = credentials.values().await;
    assert_eq!(values.len(), 2);
    assert!(values.contains_key(&old_ref));
    assert!(values.contains_key(&new_ref));
    let active = store.mirror(&added.id).await.unwrap();
    assert_eq!(active.registry, "docker.io");
    assert_eq!(active.password.as_deref(), Some("secret"));
    drop(store);

    let reopened = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    assert_eq!(reopened.mirror(&added.id).await.unwrap(), active);
    assert_eq!(credentials.values().await, before);
}

#[tokio::test]
async fn failed_main_promotion_keeps_new_registry_credential() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let path = dir.path().join("config.json");
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert_eq!(credentials.values().await.len(), 1);
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::MainPromotionDeferred; 3]
    );
}

#[tokio::test]
async fn old_credential_delete_error_after_removal_keeps_committed_update() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let old_ref = credentials.values().await.into_keys().next().unwrap();
    credentials.fail_delete_at(1).await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    let values = credentials.values().await;
    assert_eq!(values.len(), 1);
    assert!(!values.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
}

#[tokio::test]
async fn postcommit_cleanup_failure_keeps_update_when_main_is_obstructed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    credentials
        .fail_delete_and_block_config_restore(1, path.clone())
        .await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![
            StorageMaintenanceWarning::MainPromotionDeferred,
            StorageMaintenanceWarning::CredentialCleanupDeferred,
        ]
    );
    assert!(tokio::fs::metadata(path).await.unwrap().is_dir());
}

#[tokio::test]
async fn postcommit_cleanup_does_not_remove_the_obstructed_main_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    credentials
        .fail_delete_and_block_config_restore(1, path.clone())
        .await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert!(tokio::fs::metadata(path).await.unwrap().is_dir());
}

#[tokio::test]
async fn old_cleanup_error_does_not_restore_old_state_through_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let old_ref = credentials.values().await.into_keys().next().unwrap();
    credentials
        .fail_delete_and_block_config_restore(1, path.clone())
        .await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert!(!credentials.values().await.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![
            StorageMaintenanceWarning::MainPromotionDeferred,
            StorageMaintenanceWarning::CredentialCleanupDeferred,
        ]
    );
}

#[tokio::test]
async fn old_delete_error_never_attempts_to_restore_the_active_reference() {
    let dir = TempDir::new().unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(
        dir.path().join("config.json"),
        settings(),
        credentials.clone(),
    )
    .await
    .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    credentials.fail_delete_at(1).await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    assert!(
        credentials
            .values()
            .await
            .values()
            .any(|credential| credential.password == "new-secret")
    );
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
}

#[tokio::test]
async fn delete_error_with_confirmed_missing_secret_keeps_committed_mirror_removal() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let target = store
        .add_mirror(mirror("Private", "private.example", false))
        .await
        .unwrap();
    let reference = stored_credential_ref(&persisted_config(&path).await, &target.id);
    credentials.fail_delete_at(1).await;
    store.delete_mirror(&target.id).await.unwrap();
    assert!(matches!(
        store.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    let raw = tokio::fs::read_to_string(path).await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        json["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!credentials.values().await.contains_key(&reference));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
    assert!(!raw.contains("secret"));
}

#[tokio::test]
async fn delete_error_without_mutation_retains_pending_reference() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let target = store
        .add_mirror(mirror("Private", "private.example", false))
        .await
        .unwrap();
    let reference = stored_credential_ref(&persisted_config(&path).await, &target.id);
    credentials.fail_delete_without_mutation_at(1).await;
    store.delete_mirror(&target.id).await.unwrap();
    assert!(matches!(
        store.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    assert_eq!(
        persisted_config(&path).await["pendingCredentialCleanup"],
        json!([reference])
    );
    assert!(credentials.values().await.contains_key(&reference));
}

#[tokio::test]
async fn uncertain_delete_read_retains_pending_until_reopen_cleanup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let target = store
        .add_mirror(mirror("Private", "private.example", false))
        .await
        .unwrap();
    let reference = stored_credential_ref(&persisted_config(&path).await, &target.id);
    credentials.fail_delete_at(1).await;
    credentials.fail_get_after_delete_at(1).await;
    store.delete_mirror(&target.id).await.unwrap();
    assert!(matches!(
        store.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    assert_eq!(
        persisted_config(&path).await["pendingCredentialCleanup"],
        json!([reference])
    );
    assert!(!credentials.values().await.contains_key(&reference));
    drop(store);

    let reopened = ConfigStore::open(path.clone(), settings(), credentials)
        .await
        .unwrap();
    assert!(matches!(
        reopened.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    assert!(
        persisted_config(&path).await["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn windows_keyring_operations_are_dispatched_to_blocking_threads() {
    let source = include_str!("../src/credentials.rs");
    assert!(source.matches("spawn_blocking").count() >= 3);
    assert!(!source.contains("async fn get(&self, credential_ref: &str) -> CredentialResult<Option<Credential>> {\n        let entry"));
}

#[test]
fn credential_cleanup_rechecks_active_reference_immediately_before_delete() {
    let source = include_str!("../src/storage.rs");
    assert!(source.contains("self.is_active_credential_reference(&reference).await"));
}

#[test]
fn config_store_has_one_revisioned_commit_protocol() {
    let source = include_str!("../src/storage.rs");
    for retired in [
        "recover_committed_config",
        "promote_committed_recovery",
        "RegistryUpdateCompensation",
        "InactiveCredentialCompensation",
        "async fn credential_state(",
        "persist_and_swap_raw",
        "atomic_save",
    ] {
        assert!(
            !source.contains(retired),
            "retired protocol remains: {retired}"
        );
    }
    assert_eq!(
        source
            .matches("async fn commit_next_outcome_locked")
            .count(),
        1
    );
    assert!(!source.contains("tokio::fs::write(&self.inner.path"));
}

#[tokio::test]
async fn uncertain_registry_cleanup_is_persisted_then_retried() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let old_ref = credentials.values().await.into_keys().next().unwrap();
    credentials.fail_delete_at(1).await;
    credentials.fail_get_after_delete_at(1).await;
    let updated = store
        .update_mirror(
            &added.id,
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
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    let queued: serde_json::Value =
        serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
    assert_eq!(queued["pendingCredentialCleanup"], json!([old_ref]));

    store
        .update_settings(SettingsPatch {
            download_workers: Some(6),
            ..Default::default()
        })
        .await
        .unwrap();
    let cleaned: serde_json::Value =
        serde_json::from_str(&tokio::fs::read_to_string(path).await.unwrap()).unwrap();
    assert!(
        cleaned["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!credentials.values().await.contains_key(&old_ref));
}

#[tokio::test]
async fn pending_cleanup_does_not_delete_reactivated_mirror_reference() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let mirror_id = "reactivated";
    let reference = format!("mirror:{mirror_id}");
    tokio::fs::write(
        &path,
        json!({
            "version":"2.0", "settings":settings(),
            "mirrors":[
                {"id":"default","name":"Docker","registry":"docker.io","isDefault":true},
                {"id":mirror_id,"name":"Private","registry":"private.example","isDefault":false}
            ], "history":[], "pendingCredentialCleanup":[reference]
        })
        .to_string(),
    )
    .await
    .unwrap();
    let credentials = Arc::new(FailingCredentialStore::default());
    credentials.fail_delete_at(1).await;
    credentials.fail_get_after_delete_at(1).await;
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .update_mirror(
            mirror_id,
            MirrorPatch {
                username: Some(Some("alice".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.mirror(mirror_id).await.unwrap().password.as_deref(),
        Some("new-secret")
    );
    let json: serde_json::Value =
        serde_json::from_str(&tokio::fs::read_to_string(path).await.unwrap()).unwrap();
    assert!(
        json["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let refs: std::collections::HashSet<_> = json["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|mirror| mirror["credentialRef"].as_str().map(str::to_owned))
        .collect();
    let keys: std::collections::HashSet<_> = credentials.values().await.into_keys().collect();
    assert_eq!(keys, refs);
}

#[tokio::test]
async fn reopen_normalizes_active_pending_reference_before_cleanup_and_keeps_password() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let reference = "mirror:active-reference";
    tokio::fs::write(
        &path,
        json!({
            "version":"3.0", "revision":5, "transactionId":"restored-overlap",
            "settings":settings(),
            "mirrors":[{
                "id":"active", "name":"Private", "registry":"docker.io",
                "isDefault":true, "username":"alice", "credentialRef":reference
            }],
            "history":[], "pendingCredentialCleanup":[reference]
        })
        .to_string(),
    )
    .await
    .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(
            reference,
            &Credential {
                username: Some("alice".into()),
                password: "still-secret".into(),
            },
        )
        .await
        .unwrap();

    let opened = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    assert_eq!(
        opened.mirror("active").await.unwrap().password.as_deref(),
        Some("still-secret")
    );
    let normalized = persisted_config(&path).await;
    assert_eq!(normalized["revision"], 6);
    assert!(
        normalized["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drop(opened);

    let reopened = ConfigStore::open(path, settings(), credentials)
        .await
        .unwrap();
    assert_eq!(
        reopened.mirror("active").await.unwrap().password.as_deref(),
        Some("still-secret")
    );
}

#[tokio::test]
async fn pending_first_authenticated_add_set_failure_keeps_durable_cleanup_intent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    credentials.fail_set_without_mutation_at(1).await;

    let error = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::CredentialStoreFailed));
    let prepared = persisted_config(&path).await;
    assert!(prepared["mirrors"].as_array().unwrap().is_empty());
    assert_eq!(
        prepared["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(credentials.values().await.is_empty());
    assert!(
        !tokio::fs::read_to_string(path)
            .await
            .unwrap()
            .contains("secret")
    );
}

#[tokio::test]
async fn pending_first_crash_after_new_credential_write_reopens_and_cleans_orphan() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    credentials
        .block_config_recovery_after_set_at(1, path.clone())
        .await;

    let error = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::ConfigIoFailed));
    let prepared = persisted_config(&path).await;
    assert!(prepared["mirrors"].as_array().unwrap().is_empty());
    let orphan_ref = prepared["pendingCredentialCleanup"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(credentials.values().await.contains_key(&orphan_ref));
    assert!(
        !tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .contains("secret")
    );
    drop(store);

    tokio::fs::remove_dir(recovery_path(&path)).await.unwrap();
    let reopened = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    assert!(reopened.mirrors().await.unwrap().is_empty());
    assert!(credentials.values().await.is_empty());
    assert!(
        persisted_config(&path).await["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pending_first_password_update_uses_fresh_ref_and_defers_failed_old_cleanup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &added.id);
    credentials.fail_delete_without_mutation_at(1).await;

    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert_eq!(updated.password.as_deref(), Some("new-secret"));
    let activated = persisted_config(&path).await;
    let new_ref = stored_credential_ref(&activated, &added.id);
    assert_ne!(new_ref, old_ref);
    assert_eq!(activated["pendingCredentialCleanup"], json!([old_ref]));
    let stored = credentials.values().await;
    assert!(stored.contains_key(&new_ref));
    assert!(stored.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
    let raw = tokio::fs::read_to_string(path).await.unwrap();
    assert!(!raw.contains("secret"));
    assert!(!raw.contains("new-secret"));
}

#[tokio::test]
async fn pending_first_username_only_update_preserves_password_and_uses_fresh_ref() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &added.id);
    credentials.fail_delete_without_mutation_at(1).await;

    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(Some("bob".into())),
                password: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert_eq!(updated.password.as_deref(), Some("secret"));
    let activated = persisted_config(&path).await;
    let new_ref = stored_credential_ref(&activated, &added.id);
    assert_ne!(new_ref, old_ref);
    assert_eq!(activated["pendingCredentialCleanup"], json!([old_ref]));
    let values = credentials.values().await;
    assert_eq!(values[&new_ref].username.as_deref(), Some("bob"));
    assert_eq!(values[&new_ref].password, "secret");
    assert!(values.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
}

#[tokio::test]
async fn username_only_old_credential_read_failure_has_no_prepare_or_secret_write() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap();
    let before_config = tokio::fs::read(&path).await.unwrap();
    let before_credentials = credentials.values().await;
    credentials.fail_next_get().await;

    let error = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(Some("bob".into())),
                password: None,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::CredentialStoreFailed));
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before_config);
    assert_eq!(credentials.values().await, before_credentials);
    let unchanged = store.mirror(&added.id).await.unwrap();
    assert_eq!(unchanged.username.as_deref(), Some("alice"));
    assert_eq!(unchanged.password.as_deref(), Some("secret"));
    assert!(store.take_maintenance_warnings().await.is_empty());
}

#[tokio::test]
async fn pending_first_credential_removal_commits_before_failed_old_cleanup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &added.id);
    credentials.fail_delete_without_mutation_at(1).await;

    let updated = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(None),
                password: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.username, None);
    assert_eq!(updated.password, None);
    let committed = persisted_config(&path).await;
    assert!(committed["mirrors"][0].get("credentialRef").is_none());
    assert_eq!(committed["pendingCredentialCleanup"], json!([old_ref]));
    assert!(credentials.values().await.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
}

#[tokio::test]
async fn pending_first_mirror_delete_commits_before_failed_old_cleanup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let target = store
        .add_mirror(mirror("Private", "private.example", false))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &target.id);
    credentials.fail_delete_without_mutation_at(1).await;

    store.delete_mirror(&target.id).await.unwrap();

    assert!(matches!(
        store.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    let committed = persisted_config(&path).await;
    assert_eq!(committed["pendingCredentialCleanup"], json!([old_ref]));
    assert!(credentials.values().await.contains_key(&old_ref));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
}

#[tokio::test]
async fn pending_first_update_activation_failure_keeps_old_active_and_new_pending() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    let added = store
        .add_mirror(mirror("Private", "private.example", true))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &added.id);
    credentials
        .block_config_recovery_after_set_at(2, path.clone())
        .await;

    let error = store
        .update_mirror(
            &added.id,
            MirrorPatch {
                username: Some(Some("bob".into())),
                password: Some(Some("new-secret".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::ConfigIoFailed));
    let prepared = persisted_config(&path).await;
    assert_eq!(stored_credential_ref(&prepared, &added.id), old_ref);
    let new_ref = prepared["pendingCredentialCleanup"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let stored = credentials.values().await;
    assert!(stored.contains_key(&old_ref));
    assert!(stored.contains_key(&new_ref));
    assert_ne!(new_ref, old_ref);
    assert!(
        !tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .contains("new-secret")
    );
    drop(store);

    tokio::fs::remove_dir(recovery_path(&path)).await.unwrap();
    let reopened = ConfigStore::open(path, settings(), credentials.clone())
        .await
        .unwrap();
    let active = reopened.mirror(&added.id).await.unwrap();
    assert_eq!(active.username.as_deref(), Some("alice"));
    assert_eq!(active.password.as_deref(), Some("secret"));
    let reopened_values = credentials.values().await;
    assert!(reopened_values.contains_key(&old_ref));
    assert!(!reopened_values.contains_key(&new_ref));
}

#[tokio::test]
async fn pending_first_pending_clear_commit_failure_returns_committed_mirror_deletion() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let credentials = Arc::new(FailingCredentialStore::default());
    let store = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    store
        .add_mirror(mirror("Docker", "docker.io", true))
        .await
        .unwrap();
    let target = store
        .add_mirror(mirror("Private", "private.example", false))
        .await
        .unwrap();
    let old_ref = stored_credential_ref(&persisted_config(&path).await, &target.id);
    credentials.block_config_recovery_at(1, path.clone()).await;

    store.delete_mirror(&target.id).await.unwrap();

    assert!(matches!(
        store.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    assert!(!credentials.values().await.contains_key(&old_ref));
    let committed = persisted_config(&path).await;
    assert_eq!(committed["pendingCredentialCleanup"], json!([old_ref]));
    assert_eq!(
        store.take_maintenance_warnings().await,
        vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
    );
    drop(store);

    tokio::fs::remove_dir(recovery_path(&path)).await.unwrap();
    let reopened = ConfigStore::open(path.clone(), settings(), credentials.clone())
        .await
        .unwrap();
    assert!(matches!(
        reopened.mirror(&target.id).await,
        Err(AppError::MirrorNotFound)
    ));
    assert!(
        persisted_config(&path).await["pendingCredentialCleanup"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
