use docker_pull_tar_lib::domain::{
    DownloadProgress, DownloadSnapshot, DownloadStatus, ImageReference, LayerStatus, Platform,
    SettingsPatch,
};
use docker_pull_tar_lib::error::{AppError, CommandError};
use docker_pull_tar_lib::image_ref::parse_image_reference;
use std::collections::BTreeMap;

#[test]
fn parses_short_name() {
    let value = parse_image_reference("nginx:1.27", "registry-1.docker.io").unwrap();
    assert_eq!(value.registry, "registry-1.docker.io");
    assert_eq!(value.repository, "library/nginx");
    assert_eq!(value.reference, "1.27");
}

#[test]
fn trims_default_registry() {
    let value = parse_image_reference("nginx", "  registry-1.docker.io  ").unwrap();
    assert_eq!(value.registry, "registry-1.docker.io");
}

#[test]
fn validates_registry_hosts_and_ports() {
    for registry in [
        "registry.example.com",
        "registry.example.com:443",
        "localhost",
        "localhost:5000",
        "127.0.0.1",
        "127.0.0.1:5000",
    ] {
        assert!(
            parse_image_reference("team/app", registry).is_ok(),
            "rejected registry {registry:?}"
        );
    }
    for registry in [
        ".",
        ":5000",
        "registry:abc",
        "bad@host",
        "https:",
        "registry/path",
        "bad host",
        "localhost:0",
        "localhost:65536",
        "256.0.0.1",
    ] {
        assert!(
            parse_image_reference("team/app", registry).is_err(),
            "accepted registry {registry:?}"
        );
    }
}

#[test]
fn validates_repository_component_grammar() {
    for repository in [
        "team/app",
        "team/app_name",
        "team/app__name",
        "team/app.name",
        "team/app-name",
        "team/app--name",
        "team/app---name",
    ] {
        assert!(
            parse_image_reference(repository, "docker.io").is_ok(),
            "rejected repository {repository:?}"
        );
    }
    for repository in [
        "Team/App",
        "team/App",
        "team/app+name",
        "team/.app",
        "team/app.",
        "team/-app",
        "team/app-",
        "team/app___name",
        "team/app..name",
        "team/app._name",
        "team/app_-name",
        "team/app-._name",
        "team//app",
    ] {
        assert!(
            parse_image_reference(repository, "docker.io").is_err(),
            "accepted repository {repository:?}"
        );
    }
}

#[test]
fn app_settings_remains_debuggable() {
    use docker_pull_tar_lib::domain::AppSettings;
    let settings = AppSettings {
        default_output_dir: "out".into(),
        default_arch: "amd64".into(),
        default_mirror: "docker.io".into(),
        download_workers: 4,
        last_package_name: None,
    };
    assert!(format!("{settings:?}").contains("AppSettings"));
}

#[test]
fn parses_registry_with_port() {
    let value = parse_image_reference("registry.local:5000/team/app:v2", "ignored").unwrap();
    assert_eq!(value.registry, "registry.local:5000");
    assert_eq!(value.repository, "team/app");
}

#[test]
fn command_error_uses_stable_shape() {
    let json = serde_json::to_value(CommandError::from(AppError::TaskAlreadyRunning)).unwrap();
    assert_eq!(json["code"], "TASK_ALREADY_RUNNING");
    assert_eq!(json["retryable"], false);
}

#[test]
fn command_error_has_exact_detail_map_shape() {
    let error = CommandError::from(AppError::InvalidImageReference("bad".into()));
    assert_eq!(error.code, "INVALID_IMAGE_REFERENCE");
    assert_eq!(error.details.get("input"), Some(&"bad".to_string()));
    let _: BTreeMap<String, String> = error.details;
}

#[test]
fn indeterminate_config_commit_error_is_non_retryable_and_redacted() {
    let error = CommandError::from(AppError::ConfigCommitIndeterminate);

    assert_eq!(error.code, "CONFIG_COMMIT_INDETERMINATE");
    assert!(!error.retryable);
    assert!(error.details.is_empty());
}

#[test]
fn serializes_all_task_states() {
    assert_eq!(
        serde_json::to_string(&DownloadStatus::Verifying).unwrap(),
        "\"verifying\""
    );
    assert_eq!(
        serde_json::to_string(&DownloadStatus::Packing).unwrap(),
        "\"packing\""
    );
    assert_eq!(
        serde_json::to_string(&LayerStatus::Cancelled).unwrap(),
        "\"cancelled\""
    );
}

#[test]
fn snapshot_contains_task_and_lifecycle_fields() {
    let snapshot = DownloadSnapshot {
        task_id: "task-1".into(),
        image: ImageReference {
            registry: "docker.io".into(),
            repository: "library/nginx".into(),
            reference: "latest".into(),
        },
        status: DownloadStatus::Preparing,
        progress: DownloadProgress {
            current_layer: 0,
            total_layers: 0,
            downloaded_bytes: 0,
            total_bytes: 0,
            speed: 0.0,
            layers: vec![],
        },
        started_at: Some("2026-07-13T00:00:00Z".into()),
        finished_at: None,
        last_error: None,
    };
    let json = serde_json::to_value(snapshot).unwrap();
    assert_eq!(json["taskId"], "task-1");
    assert_eq!(json["image"]["repository"], "library/nginx");
    assert!(json.get("startedAt").is_some());
    assert!(json.get("finishedAt").is_some());
    assert!(json.get("lastError").is_some());
}

#[test]
fn rejects_empty_input() {
    let error = parse_image_reference("  ", "registry-1.docker.io").unwrap_err();
    assert!(matches!(error, AppError::InvalidImageReference(_)));
}

#[test]
fn rejects_invalid_separators() {
    for input in ["/nginx", "nginx/", "team//app", "nginx:", "nginx@"] {
        assert!(
            matches!(
                parse_image_reference(input, "registry-1.docker.io"),
                Err(AppError::InvalidImageReference(_))
            ),
            "accepted {input:?}"
        );
    }
}

#[test]
fn preserves_digest_reference() {
    let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let value = parse_image_reference(
        &format!("ghcr.io/acme/app@{digest}"),
        "registry-1.docker.io",
    )
    .unwrap();
    assert_eq!(value.registry, "ghcr.io");
    assert_eq!(value.repository, "acme/app");
    assert_eq!(value.reference, digest);
}

#[test]
fn rejects_invalid_digest_payloads() {
    for digest in [
        "sha256:0123456789abcdef",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
    ] {
        assert!(parse_image_reference(&format!("acme/app@{digest}"), "docker.io").is_err());
    }
}

#[test]
fn enforces_tag_grammar_and_length() {
    for tag in [".bad", "-bad", "bad/tag", "bad+tag", ""] {
        assert!(parse_image_reference(&format!("nginx:{tag}"), "docker.io").is_err());
    }
    let long_tag = "a".repeat(129);
    assert!(parse_image_reference(&format!("nginx:{long_tag}"), "docker.io").is_err());
    for tag in ["good_tag", "v1.2-rc1", "a"] {
        assert!(parse_image_reference(&format!("nginx:{tag}"), "docker.io").is_ok());
    }
}

#[test]
fn credential_debug_output_is_redacted() {
    use docker_pull_tar_lib::domain::{DownloadStartRequest, Mirror, MirrorInput, MirrorPatch};
    let mirror = Mirror {
        id: "id".into(),
        name: "name".into(),
        registry: "docker.io".into(),
        is_default: false,
        username: Some("user".into()),
        password: Some("secret".into()),
    };
    let input = MirrorInput {
        name: "name".into(),
        registry: "docker.io".into(),
        is_default: false,
        username: Some("user".into()),
        password: Some("secret".into()),
    };
    let patch = MirrorPatch {
        name: None,
        registry: None,
        is_default: None,
        username: Some(Some("user".into())),
        password: Some(Some("secret".into())),
    };
    let request = DownloadStartRequest {
        package_name: "nginx".into(),
        tag: "latest".into(),
        arch: "amd64".into(),
        mirror: "docker.io".into(),
        output_dir: "out".into(),
        username: Some("user".into()),
        password: Some("secret".into()),
    };
    for debug in [
        format!("{mirror:?}"),
        format!("{input:?}"),
        format!("{patch:?}"),
        format!("{request:?}"),
    ] {
        assert!(!debug.contains("secret"), "debug leaked secret: {debug}");
    }
}

#[test]
fn serializes_arm_v7_platform_in_camel_case() {
    let json = serde_json::to_value(Platform {
        os: "linux".into(),
        architecture: "arm".into(),
        variant: Some("v7".into()),
    })
    .unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "os": "linux",
            "architecture": "arm",
            "variant": "v7"
        })
    );
}

#[test]
fn settings_patch_distinguishes_missing_from_null() {
    let missing: SettingsPatch = serde_json::from_value(serde_json::json!({})).unwrap();
    let cleared: SettingsPatch =
        serde_json::from_value(serde_json::json!({ "lastPackageName": null })).unwrap();
    let replaced: SettingsPatch =
        serde_json::from_value(serde_json::json!({ "lastPackageName": "nginx" })).unwrap();

    assert_eq!(missing.last_package_name, None);
    assert_eq!(cleared.last_package_name, Some(None));
    assert_eq!(replaced.last_package_name, Some(Some("nginx".into())));
}
