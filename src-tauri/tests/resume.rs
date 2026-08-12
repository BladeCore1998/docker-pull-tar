use docker_pull_tar_lib::blob::{BlobDownloader, BlobProgress, BlobRequest};
use docker_pull_tar_lib::domain::{Descriptor, ImageReference, Platform, ResumeMetadata};
use docker_pull_tar_lib::error::AppError;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

struct NoResumeHeaders;

impl Match for NoResumeHeaders {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("range") && !request.headers.contains_key("if-range")
    }
}

#[derive(Clone)]
struct BoundedRanges {
    calls: Arc<AtomicUsize>,
}

impl Respond for BoundedRanges {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let range = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok());
        match (self.calls.fetch_add(1, Ordering::SeqCst), range) {
            (0, Some("bytes=5-")) => ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 5-7/11")
                .set_body_bytes(b" wo"),
            (1, Some("bytes=8-")) => ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 8-10/11")
                .set_body_bytes(b"rld"),
            _ => ResponseTemplate::new(400),
        }
    }
}

#[derive(Clone)]
struct Invalid416ThenFresh {
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Any416ThenFresh {
    calls: Arc<AtomicUsize>,
}

impl Respond for Any416ThenFresh {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(416)
        } else {
            ResponseTemplate::new(200).set_body_bytes(b"hello world")
        }
    }
}

impl Respond for Invalid416ThenFresh {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let has_range = request.headers.contains_key("range");
        match (self.calls.fetch_add(1, Ordering::SeqCst), has_range) {
            (0, true) => ResponseTemplate::new(416),
            (1, false) => ResponseTemplate::new(200).set_body_bytes(b"hello world"),
            _ => ResponseTemplate::new(400),
        }
    }
}

fn digest(body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(body))
}

fn request(server: &MockServer, body: &[u8]) -> BlobRequest {
    BlobRequest {
        url: format!("{}/blob", server.uri()).parse().unwrap(),
        image: ImageReference {
            registry: server.address().to_string(),
            repository: "acme/app".into(),
            reference: "latest".into(),
        },
        platform: Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
        manifest_digest: "sha256:manifest-a".into(),
        descriptor: Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: digest(body),
            size: body.len() as u64,
        },
        authorization: None,
    }
}

fn names(dir: &TempDir, body: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
    let stem = digest(body).replace(':', "_");
    (
        dir.path()
            .join(".docker-pull-tar")
            .join(format!("{stem}.part")),
        dir.path()
            .join(".docker-pull-tar")
            .join(format!("{stem}.resume.json")),
    )
}

async fn partial(dir: &TempDir, request: &BlobRequest, bytes: &[u8], etag: &str) {
    let stem = request.descriptor.digest.replace(':', "_");
    let part = dir
        .path()
        .join(".docker-pull-tar")
        .join(format!("{stem}.part"));
    let metadata = dir
        .path()
        .join(".docker-pull-tar")
        .join(format!("{stem}.resume.json"));
    tokio::fs::create_dir_all(part.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(part, bytes).await.unwrap();
    let data = ResumeMetadata {
        version: 1,
        image: request.image.clone(),
        platform: request.platform.clone(),
        manifest_digest: request.manifest_digest.clone(),
        descriptor: request.descriptor.clone(),
        etag: Some(etag.into()),
        downloaded: bytes.len() as u64,
    };
    tokio::fs::write(metadata, serde_json::to_vec(&data).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn valid_206_appends_with_if_range_and_counts_existing_bytes_only_in_downloaded() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .and(path("/blob"))
        .and(header("range", "bytes=5-"))
        .and(header("if-range", "\"v1\""))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 5-10/11")
                .insert_header("etag", "\"v1\"")
                .set_body_bytes(&body[5..]),
        )
        .mount(&server)
        .await;
    let events = Arc::new(Mutex::new(Vec::<BlobProgress>::new()));
    let captured = events.clone();

    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), move |p| {
            captured.lock().unwrap().push(p)
        })
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
    let final_progress = events.lock().unwrap().last().copied().unwrap();
    assert_eq!(final_progress.downloaded, 11);
    assert_eq!(final_progress.network_bytes, 6);
    assert!(final_progress.network_speed > 0.0);
}

#[tokio::test]
async fn server_200_ignoring_range_restarts_instead_of_appending() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .and(header("range", "bytes=5-"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn complete_valid_part_is_promoted_after_416() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, body, "\"v1\"").await;
    Mock::given(method("GET"))
        .and(header("range", "bytes=11-"))
        .respond_with(ResponseTemplate::new(416))
        .expect(1)
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn invalid_416_discards_partial_and_restarts_without_range() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(Invalid416ThenFresh {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn corrupt_complete_part_is_discarded_after_416_and_restarted() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"HELLO WORLD", "\"v1\"").await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(Invalid416ThenFresh {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn metadata_mismatch_followed_by_416_retries_clean_from_zero() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let mut req = request(&server, body);
    partial(&dir, &req, b"hello", "\"stale\"").await;
    req.platform.architecture = "arm64".into();
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(NoResumeHeaders)
        .respond_with(Any416ThenFresh {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn oversized_partial_is_invalidated_before_requesting() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello world extra", "\"v1\"").await;
    Mock::given(method("GET"))
        .and(NoResumeHeaders)
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn bounded_206_ranges_continue_from_each_validated_end() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(BoundedRanges {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

async fn assert_bad_range(content_range: &str, body: &[u8]) {
    let server = MockServer::start().await;
    let expected = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, expected);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", content_range)
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::ResumeNotSupported));
}

#[tokio::test]
async fn malformed_content_range_is_rejected() {
    assert_bad_range("bytes nope", b" world").await;
}

#[tokio::test]
async fn content_range_end_before_start_is_rejected() {
    assert_bad_range("bytes 5-4/11", b"").await;
}

#[tokio::test]
async fn content_range_total_must_match_descriptor() {
    assert_bad_range("bytes 5-10/12", b" world").await;
}

#[tokio::test]
async fn content_range_body_length_must_match_declared_span() {
    assert_bad_range("bytes 5-9/11", b" world").await;
}

#[tokio::test]
async fn wrong_content_range_is_rejected_without_appending() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 4-10/11")
                .set_body_bytes(&body[5..]),
        )
        .mount(&server)
        .await;
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::ResumeNotSupported));
    assert_eq!(
        tokio::fs::read(names(&dir, body).0).await.unwrap(),
        b"hello"
    );
}

#[tokio::test]
async fn cancellation_preserves_part_for_a_future_resume() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 5-10/11")
                .set_body_bytes(&body[5..]),
        )
        .mount(&server)
        .await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &cancellation, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::TaskCancelled));
    assert_eq!(
        tokio::fs::read(names(&dir, body).0).await.unwrap(),
        b"hello"
    );
}

#[tokio::test]
async fn digest_mismatch_removes_corrupt_partial_content() {
    let server = MockServer::start().await;
    let expected = b"hello world";
    let corrupt = b"HELLO WORLD";
    let dir = TempDir::new().unwrap();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(corrupt))
        .mount(&server)
        .await;
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(
            request(&server, expected),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::DigestMismatch));
    assert!(!names(&dir, expected).0.exists());
    assert!(!names(&dir, expected).1.exists());
}

#[tokio::test]
async fn manifest_digest_change_invalidates_partial_and_metadata() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let mut old = request(&server, body);
    partial(&dir, &old, b"hello", "\"v1\"").await;
    old.manifest_digest = "sha256:manifest-b".into();
    Mock::given(method("GET"))
        .and(NoResumeHeaders)
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(old, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn image_metadata_mismatch_does_not_send_resume_headers() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let mut req = request(&server, body);
    partial(&dir, &req, b"hello", "\"stale\"").await;
    req.image.repository = "other/app".into();
    Mock::given(method("GET"))
        .and(NoResumeHeaders)
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
}

#[tokio::test]
async fn platform_metadata_mismatch_does_not_send_resume_headers() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let mut req = request(&server, body);
    partial(&dir, &req, b"hello", "\"stale\"").await;
    req.platform.architecture = "arm64".into();
    Mock::given(method("GET"))
        .and(NoResumeHeaders)
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_after_a_network_chunk_preserves_new_bytes_and_metadata() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 5-10/11")
                .set_body_bytes(&body[5..]),
        )
        .mount(&server)
        .await;
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &cancellation, move |_| signal.cancel())
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::TaskCancelled));
    assert_eq!(tokio::fs::read(names(&dir, body).0).await.unwrap(), body);
    let metadata: ResumeMetadata =
        serde_json::from_slice(&tokio::fs::read(names(&dir, body).1).await.unwrap()).unwrap();
    assert_eq!(metadata.downloaded, body.len() as u64);
}

#[tokio::test]
async fn resumed_speed_is_calculated_from_network_bytes_not_validated_prefix() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let req = request(&server, body);
    partial(&dir, &req, b"hello", "\"v1\"").await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 5-10/11")
                .set_body_bytes(&body[5..]),
        )
        .mount(&server)
        .await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), move |progress| {
            captured.lock().unwrap().push(progress)
        })
        .await
        .unwrap();
    let progress = *events.lock().unwrap().last().unwrap();
    assert_eq!(progress.downloaded, 11);
    assert_eq!(progress.network_bytes, 6);
    assert!(progress.network_speed <= progress.network_bytes as f64 * 1000.0);
}

#[tokio::test]
async fn promotion_failure_retains_preexisting_target() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let stem = digest(body).replace(':', "_");
    let blobs = dir.path().join("blobs");
    tokio::fs::create_dir_all(&blobs).await.unwrap();
    let target = blobs.join(&stem);
    tokio::fs::create_dir(&target).await.unwrap();
    tokio::fs::write(target.join("old-marker"), b"old target")
        .await
        .unwrap();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(request(&server, body), &CancellationToken::new(), |_| {})
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::OutputNotWritable));
    assert_eq!(
        tokio::fs::read(target.join("old-marker")).await.unwrap(),
        b"old target"
    );
    assert_eq!(tokio::fs::read(names(&dir, body).0).await.unwrap(), body);
}

#[tokio::test]
async fn cancellation_before_send_makes_no_network_request() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(request(&server, body), &cancellation, |_| {})
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::TaskCancelled));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_during_prefix_validation_stops_before_network() {
    let server = MockServer::start().await;
    let body = vec![b'x'; 256 * 1024];
    let dir = TempDir::new().unwrap();
    let req = request(&server, &body);
    partial(&dir, &req, &body[..128 * 1024], "\"v1\"").await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &cancellation, |_| {})
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::TaskCancelled));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        tokio::fs::metadata(names(&dir, &body).0)
            .await
            .unwrap()
            .len(),
        128 * 1024
    );
}

#[tokio::test]
async fn committed_blob_succeeds_when_post_commit_metadata_cleanup_fails() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let paths = names(&dir, body);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let metadata = paths.1.clone();
    let sabotage = Arc::new(AtomicUsize::new(0));
    let calls = sabotage.clone();

    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(
            request(&server, body),
            &CancellationToken::new(),
            move |_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::fs::remove_file(&metadata).unwrap();
                    std::fs::create_dir(&metadata).unwrap();
                    std::fs::write(metadata.join("blocked"), b"keep").unwrap();
                }
            },
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
    let marker = paths.1.with_extension("cleanup-pending");
    assert!(marker.exists());
}

#[tokio::test]
async fn authorization_is_sent_but_never_in_an_error() {
    let server = MockServer::start().await;
    let body = b"hello world";
    let dir = TempDir::new().unwrap();
    let token = "Bearer extremely-secret";
    let mut req = request(&server, body);
    req.authorization = Some(token.into());
    Mock::given(method("GET"))
        .and(header("authorization", token))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let error = BlobDownloader::new(dir.path())
        .unwrap()
        .download(req, &CancellationToken::new(), |_| {})
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("extremely-secret"));
    assert!(!format!("{error:?}").contains("extremely-secret"));
}
