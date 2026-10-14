//! Opt-in network verification using the same pipeline as the desktop application.
//! Set DOCKER_PULL_LIVE_REGISTRY, then run this test with --ignored --nocapture.
use docker_pull_tar_lib::batch_manager::BatchManager;
use docker_pull_tar_lib::credentials::MemoryCredentialStore;
use docker_pull_tar_lib::domain::{
    AppSettings, BatchStartRequest, BatchStatus, DownloadStartRequest, DownloadStatus,
};
use docker_pull_tar_lib::download_pipeline::{
    DownloadCoordinator, DownloadService, ProductionDownloadPipeline,
};
use docker_pull_tar_lib::events::{DOWNLOAD_LOG_EVENT, DOWNLOAD_PROGRESS_EVENT, EventSink};
use docker_pull_tar_lib::storage::ConfigStore;
use docker_pull_tar_lib::task_manager::TaskManager;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

#[derive(Default)]
struct RecordedEvents(Mutex<Vec<Value>>);

impl EventSink for RecordedEvents {
    fn emit(&self, name: &'static str, payload: Value) {
        if name == DOWNLOAD_LOG_EVENT {
            println!("{}: {}", payload["taskId"], payload["message"]);
        }
        self.0
            .lock()
            .unwrap()
            .push(json!({ "event": name, "payload": payload }));
    }
}

fn verify_archive(path: &Path) {
    let mut entries = BTreeMap::new();
    let mut archive = tar::Archive::new(std::fs::File::open(path).unwrap());
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        entries.insert(name, bytes);
    }
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest.as_array().unwrap().len(), 1);
    let config_name = manifest[0]["Config"].as_str().unwrap();
    let config_bytes = &entries[config_name];
    assert_eq!(
        format!("{:x}.json", Sha256::digest(config_bytes)),
        config_name
    );
    let config: Value = serde_json::from_slice(config_bytes).unwrap();
    assert_eq!(config["architecture"], "amd64");
    assert_eq!(config["os"], "linux");
    let layers = manifest[0]["Layers"].as_array().unwrap();
    let diff_ids = config["rootfs"]["diff_ids"].as_array().unwrap();
    assert!(!layers.is_empty());
    assert_eq!(layers.len(), diff_ids.len());
    for (layer, diff_id) in layers.iter().zip(diff_ids) {
        let bytes = &entries[layer.as_str().unwrap()];
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(bytes)),
            diff_id.as_str().unwrap()
        );
    }
}

fn matching<'a>(
    events: &'a [Value],
    name: &'static str,
    task_id: &str,
) -> impl Iterator<Item = &'a Value> {
    events
        .iter()
        .filter(move |event| event["event"] == name && event["payload"]["taskId"] == task_id)
}

fn verify_events(events: &[Value], task_id: &str) {
    assert!(
        matching(events, DOWNLOAD_PROGRESS_EVENT, task_id)
            .any(|event| { event["payload"]["downloadedBytes"].as_u64().unwrap_or(0) > 0 })
    );
    let logs: Vec<_> = matching(events, DOWNLOAD_LOG_EVENT, task_id).collect();
    assert!(logs.len() >= 3);
    assert!(
        logs.iter()
            .all(|event| event["payload"]["timestamp"].is_string())
    );
    assert_eq!(
        logs.iter()
            .filter(|event| event["payload"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Download completed:"))
            .count(),
        1
    );
    assert!(
        !logs
            .iter()
            .any(|event| event["payload"]["level"] == "error")
    );
}

#[tokio::test]
#[ignore = "requires an accessible public registry; run explicitly for release verification"]
async fn real_single_and_batch_downloads_produce_valid_archives_and_task_logs() {
    let registry = std::env::var("DOCKER_PULL_LIVE_REGISTRY")
        .unwrap_or_else(|_| "registry-1.docker.io".into());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/live-download")
        .join(Uuid::new_v4().to_string());
    let output = root.join("archives");
    std::fs::create_dir_all(&output).unwrap();
    println!("Verification artifacts: {}", root.display());
    let config = ConfigStore::open(
        root.join("config.json"),
        AppSettings {
            default_output_dir: output.to_string_lossy().into_owned(),
            default_arch: "amd64".into(),
            default_mirror: registry.clone(),
            download_workers: 4,
            last_package_name: None,
        },
        Arc::new(MemoryCredentialStore::default()),
    )
    .await
    .unwrap();
    let pipeline = Arc::new(ProductionDownloadPipeline::from_config(config.clone()).unwrap());
    let events = Arc::new(RecordedEvents::default());
    let downloads = DownloadCoordinator::new(
        config.clone(),
        TaskManager::new(pipeline.clone(), events.clone(), Arc::new(config.clone())),
    );
    let request = |name: &str| DownloadStartRequest {
        package_name: name.into(),
        tag: "latest".into(),
        arch: "amd64".into(),
        mirror: registry.clone(),
        output_dir: output.to_string_lossy().into_owned(),
        username: None,
        password: None,
    };
    downloads.start(request("hello-world")).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let snapshot = downloads.status().await;
            if matches!(
                snapshot.status,
                DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Cancelled
            ) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if result.is_err() {
        downloads.cancel().await;
    }
    let snapshot = result.expect("single download timed out");
    std::fs::write(
        root.join("single-events.json"),
        serde_json::to_vec_pretty(&*events.0.lock().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshot.status,
        DownloadStatus::Completed,
        "{:?}",
        snapshot.last_error
    );
    verify_events(&events.0.lock().unwrap(), &snapshot.task_id);

    let batches = BatchManager::new(
        config.clone(),
        pipeline,
        events.clone(),
        Arc::new(config.clone()),
    );
    let batch = batches
        .start(BatchStartRequest {
            images: vec![request("hello-world"), request("busybox")],
            max_concurrent_images: 2,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let snapshot = batches.status(&batch.batch_id).await.unwrap();
            if !matches!(
                snapshot.status,
                BatchStatus::Queued | BatchStatus::Running | BatchStatus::Cancelling
            ) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if result.is_err() {
        batches.cancel(&batch.batch_id).await.unwrap();
    }
    let batch = result.expect("batch download timed out");
    let recorded = events.0.lock().unwrap().clone();
    std::fs::write(
        root.join("all-events.json"),
        serde_json::to_vec_pretty(&recorded).unwrap(),
    )
    .unwrap();
    assert_eq!(batch.status, BatchStatus::Completed, "{batch:?}");
    for task in &batch.tasks {
        verify_events(&recorded, &task.task_id);
    }
    drop(recorded);
    let history = config.history().await;
    assert_eq!(history.len(), 3);
    for item in &history {
        assert_eq!(item.status, DownloadStatus::Completed);
        let path = Path::new(item.output_path.as_ref().unwrap());
        assert!(path.metadata().unwrap().len() > 0);
        verify_archive(path);
        println!("Verified archive: {}", path.display());
    }
}
