use docker_pull_tar_lib::blob::{BlobDownloader, BlobRequest};
use docker_pull_tar_lib::domain::{Descriptor, ImageReference, Platform};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest(body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(body))
}

fn request(server: &MockServer, body: &[u8]) -> BlobRequest {
    BlobRequest {
        url: format!("{}/v2/acme/app/blobs/{}", server.uri(), digest(body))
            .parse()
            .unwrap(),
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
        manifest_digest: "sha256:manifest".into(),
        descriptor: Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: digest(body),
            size: body.len() as u64,
        },
        authorization: None,
    }
}

#[tokio::test]
async fn fresh_200_is_verified_and_atomically_promoted() {
    let server = MockServer::start().await;
    let body = b"fresh blob";
    Mock::given(method("GET"))
        .and(path(format!("/v2/acme/app/blobs/{}", digest(body))))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();

    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(request(&server, body), &CancellationToken::new(), |_| {})
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&output).await.unwrap(), body);
    let expected_name = digest(body).replace(':', "_");
    assert_eq!(
        output.file_name().unwrap(),
        std::ffi::OsStr::new(&expected_name)
    );
    assert!(
        !dir.path()
            .join(".docker-pull-tar")
            .join(format!("{}.part", digest(body).replace(':', "_")))
            .exists()
    );
}

#[tokio::test]
async fn empty_blob_is_verified_without_a_network_request() {
    let server = MockServer::start().await;
    let body = b"";
    let dir = TempDir::new().unwrap();

    let output = BlobDownloader::new(dir.path())
        .unwrap()
        .download(request(&server, body), &CancellationToken::new(), |_| {})
        .await
        .unwrap();

    assert_eq!(tokio::fs::metadata(output).await.unwrap().len(), 0);
}
