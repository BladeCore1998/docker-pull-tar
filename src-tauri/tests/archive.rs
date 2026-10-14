use docker_pull_tar_lib::archive::{ArchiveRequest, build_archive, estimate_unpacked_size};
use docker_pull_tar_lib::domain::{Descriptor, Platform, ResolvedImage};
use docker_pull_tar_lib::error::AppError;
use flate2::{Compression, write::GzEncoder};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::Path,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GZIP_MEDIA: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const TAR_MEDIA: &str = "application/vnd.docker.image.rootfs.diff.tar";
const CONFIG_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GZIP_DIGEST: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TAR_DIGEST: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const IMAGE_DIGEST: &str =
    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const FIRST_ID: &str = "a269278c0055067ae7f82e1bcf673a04d257523e680ca5d9cdb126742e258a42";
const SECOND_ID: &str = "50ef0eb310b4807e72aa7c67de37bd3c387b5a032fd6ddd854af88339e6fd57b";

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .trim()
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error)
            if name == "uncompressed-layer.tar" && error.kind() == std::io::ErrorKind::NotFound =>
        {
            // Keep this fixture deterministic without committing a binary file that
            // is intentionally ignored by the repository's `*.tar` rule.
            b"uncompressed-layer-content\n".to_vec()
        }
        Err(error) => panic!("failed to read fixture {name}: {error}"),
    };
    if name.ends_with(".hex") {
        decode_hex(std::str::from_utf8(&bytes).unwrap())
    } else {
        bytes
    }
}

fn descriptor(media_type: &str, digest: &str, size: usize) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: digest.into(),
        size: size as u64,
    }
}

fn resolved(gzip_size: usize, tar_size: usize) -> ResolvedImage {
    ResolvedImage {
        registry: "registry.example".into(),
        repository: "acme/widget".into(),
        reference: "v1".into(),
        manifest_digest: "sha256:manifest".into(),
        config: descriptor(
            "application/vnd.oci.image.config.v1+json",
            CONFIG_DIGEST,
            fixture("config.json").len(),
        ),
        layers: vec![
            descriptor(GZIP_MEDIA, GZIP_DIGEST, gzip_size),
            descriptor(TAR_MEDIA, TAR_DIGEST, tar_size),
        ],
        platform: Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
    }
}

fn setup(dir: &TempDir) -> ArchiveRequest {
    let config = fixture("config.json");
    let gzip = fixture("gzip-layer.tar.gz.hex");
    let plain = fixture("uncompressed-layer.tar");
    let config_path = dir.path().join("verified-config");
    let gzip_path = dir.path().join("verified-gzip-blob");
    let plain_path = dir.path().join("verified-plain-blob");
    std::fs::write(&config_path, config).unwrap();
    std::fs::write(&gzip_path, &gzip).unwrap();
    std::fs::write(&plain_path, &plain).unwrap();
    ArchiveRequest {
        image: resolved(gzip.len(), plain.len()),
        config_path,
        layer_paths: vec![gzip_path, plain_path],
        output_dir: dir.path().join("output"),
        output_name: "acme_widget_v1".into(),
    }
}

fn read_tar(path: &Path) -> (Vec<String>, BTreeMap<String, Vec<u8>>) {
    let mut names = Vec::new();
    let mut contents = BTreeMap::new();
    for entry in tar::Archive::new(File::open(path).unwrap())
        .entries()
        .unwrap()
    {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().replace('\\', "/");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        names.push(name.clone());
        contents.insert(name, bytes);
    }
    (names, contents)
}

#[tokio::test]
async fn estimates_actual_gzip_and_uncompressed_sizes_without_expansion_factor() {
    let dir = TempDir::new().unwrap();
    let unpacked = vec![b'x'; 256 * 1024];
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&unpacked).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(unpacked.len() > compressed.len() * 3);
    let plain = vec![b'y'; 4096];
    let gzip_path = dir.path().join("high-ratio.tar.gz");
    let plain_path = dir.path().join("plain.tar");
    std::fs::write(&gzip_path, &compressed).unwrap();
    std::fs::write(&plain_path, &plain).unwrap();
    let image = resolved(compressed.len(), plain.len());

    let estimate =
        estimate_unpacked_size(&image, &[gzip_path, plain_path], &CancellationToken::new())
            .await
            .unwrap();

    assert_eq!(estimate, (unpacked.len() + plain.len()) as u64);
}

#[tokio::test]
async fn unpacked_size_estimate_honors_cancellation_before_reading() {
    let dir = TempDir::new().unwrap();
    let request = setup(&dir);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(matches!(
        estimate_unpacked_size(&request.image, &request.layer_paths, &cancellation).await,
        Err(AppError::TaskCancelled)
    ));
}

#[tokio::test]
async fn builds_legacy_archive_in_manifest_order_with_parent_chain() {
    let dir = TempDir::new().unwrap();
    let request = setup(&dir);
    let result = build_archive(request, &CancellationToken::new())
        .await
        .unwrap();
    let first = FIRST_ID;
    let second = SECOND_ID;
    let config_name = format!("{}.json", CONFIG_DIGEST.trim_start_matches("sha256:"));
    let (names, files) = read_tar(&result.path);

    assert_eq!(result.size, std::fs::metadata(&result.path).unwrap().len());
    assert_eq!(
        names,
        vec![
            config_name.clone(),
            "manifest.json".into(),
            "repositories".into(),
            format!("{FIRST_ID}/json"),
            format!("{FIRST_ID}/layer.tar"),
            format!("{SECOND_ID}/json"),
            format!("{SECOND_ID}/layer.tar"),
        ]
    );
    assert_eq!(files[&config_name], fixture("config.json"));
    let manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
    assert_eq!(manifest[0]["Config"], config_name);
    assert_eq!(
        manifest[0]["RepoTags"],
        serde_json::json!(["acme/widget:v1"])
    );
    assert_eq!(
        manifest[0]["Layers"],
        serde_json::json!([
            format!("{FIRST_ID}/layer.tar"),
            format!("{SECOND_ID}/layer.tar")
        ])
    );
    let first_json: Value = serde_json::from_slice(&files[&format!("{first}/json")]).unwrap();
    let second_json: Value = serde_json::from_slice(&files[&format!("{second}/json")]).unwrap();
    assert_eq!(
        first_json,
        serde_json::json!({"id": FIRST_ID, "parent": null})
    );
    assert_eq!(
        second_json,
        serde_json::json!({"id": SECOND_ID, "parent": FIRST_ID})
    );
    let repositories: Value = serde_json::from_slice(&files["repositories"]).unwrap();
    assert_eq!(
        repositories,
        serde_json::json!({"acme/widget":{"v1":SECOND_ID}})
    );
}

#[tokio::test]
async fn digest_reference_builds_an_untagged_docker_archive() {
    let dir = TempDir::new().unwrap();
    let mut request = setup(&dir);
    request.image.reference = IMAGE_DIGEST.into();

    let result = build_archive(request, &CancellationToken::new())
        .await
        .unwrap();
    let (_, files) = read_tar(&result.path);
    let manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
    let repositories: Value = serde_json::from_slice(&files["repositories"]).unwrap();

    assert!(manifest[0]["RepoTags"].is_null());
    assert_eq!(repositories, serde_json::json!({}));
    assert!(!String::from_utf8_lossy(&files["manifest.json"]).contains(IMAGE_DIGEST));
    assert!(!String::from_utf8_lossy(&files["repositories"]).contains(IMAGE_DIGEST));
}

#[tokio::test]
async fn decompresses_gzip_layers_and_copies_uncompressed_layers_exactly() {
    let dir = TempDir::new().unwrap();
    let mut request = setup(&dir);
    request.image.layers[0].media_type = "application/vnd.docker.image.rootfs.diff.tar.gzip".into();
    request.image.layers[1].media_type = "application/vnd.oci.image.layer.v1.tar".into();
    let result = build_archive(request, &CancellationToken::new())
        .await
        .unwrap();
    let (_, files) = read_tar(&result.path);
    assert_eq!(
        files[&format!("{FIRST_ID}/layer.tar")],
        b"gzip-layer-content\n"
    );
    assert_eq!(
        files[&format!("{SECOND_ID}/layer.tar")],
        fixture("uncompressed-layer.tar")
    );
}

#[tokio::test]
async fn rejects_unknown_layer_media_type_before_creating_output() {
    let dir = TempDir::new().unwrap();
    let mut request = setup(&dir);
    request.image.layers[0].media_type = "application/vnd.oci.image.layer.v1.tar+zstd".into();
    let error = build_archive(request.clone(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "UNSUPPORTED_LAYER_MEDIA_TYPE");
    assert!(!request.output_dir.exists());
}

#[tokio::test]
async fn cancellation_leaves_no_final_or_temporary_archive() {
    let dir = TempDir::new().unwrap();
    let mut request = setup(&dir);
    let large_layer = dir.path().join("large-layer");
    File::create(&large_layer)
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    request.layer_paths[1] = large_layer;
    let cancellation = CancellationToken::new();
    let build_request = request.clone();
    let build_cancellation = cancellation.clone();
    let build =
        tokio::spawn(async move { build_archive(build_request, &build_cancellation).await });
    loop {
        assert!(!build.is_finished(), "archive finished before cancellation");
        if request.output_dir.exists()
            && std::fs::read_dir(&request.output_dir)
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    let error = build.await.unwrap().unwrap_err();
    assert!(matches!(error, AppError::TaskCancelled));
    if request.output_dir.exists() {
        assert_eq!(std::fs::read_dir(request.output_dir).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn uses_collision_safe_name_without_overwriting_existing_archive() {
    let dir = TempDir::new().unwrap();
    let request = setup(&dir);
    std::fs::create_dir_all(&request.output_dir).unwrap();
    let existing = request.output_dir.join("acme_widget_v1.tar");
    std::fs::write(&existing, b"keep me").unwrap();

    let result = build_archive(request, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(std::fs::read(existing).unwrap(), b"keep me");
    assert_eq!(result.path.file_name().unwrap(), "acme_widget_v1-1.tar");
}

#[tokio::test]
async fn rejects_unsafe_output_names_and_emits_only_relative_safe_entries() {
    for name in ["../escape", "/absolute", r"C:\absolute", "nested/name"] {
        let dir = TempDir::new().unwrap();
        let mut request = setup(&dir);
        request.output_name = name.into();
        let error = build_archive(request, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "OUTPUT_NOT_WRITABLE", "accepted {name:?}");
    }

    let dir = TempDir::new().unwrap();
    let result = build_archive(setup(&dir), &CancellationToken::new())
        .await
        .unwrap();
    let (names, _) = read_tar(&result.path);
    assert!(names.iter().all(|name| {
        !name.starts_with('/') && !name.starts_with("../") && !name.contains("/../")
    }));
}

#[tokio::test]
async fn identical_verified_inputs_produce_identical_archive_bytes() {
    let first_dir = TempDir::new().unwrap();
    let second_dir = TempDir::new().unwrap();
    let first = build_archive(setup(&first_dir), &CancellationToken::new())
        .await
        .unwrap();
    let second = build_archive(setup(&second_dir), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(first.path).unwrap(),
        std::fs::read(second.path).unwrap()
    );
}
