use docker_pull_tar_lib::domain::{ImageReference, Platform};
use docker_pull_tar_lib::error::AppError;
use docker_pull_tar_lib::registry::{RegistryClient, parse_bearer_challenge, validate_token_realm};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[cfg(feature = "test-utils")]
use wiremock::matchers::basic_auth;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

struct NoAuthorization;

impl Match for NoAuthorization {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

const MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.manifest.v1+json",
  "config": {"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","size":10},
  "layers": [
    {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","size":20},
    {"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","size":30}
  ]
}"#;

fn digest(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

fn index(media_type: &str, manifest_digest: &str) -> String {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": media_type,
        "manifests": [
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":MANIFEST.len(),"platform":{"os":"linux","architecture":"amd64"}},
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":MANIFEST.len(),"platform":{"os":"linux","architecture":"arm64"}},
            {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":manifest_digest,"size":MANIFEST.len(),"platform":{"os":"linux","architecture":"arm","variant":"v7"}}
        ]
    }).to_string()
}

#[derive(Clone)]
struct RetryThen {
    attempts: Arc<AtomicUsize>,
    retry_status: u16,
    success: ResponseTemplate,
}

impl Respond for RetryThen {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(self.retry_status)
        } else {
            self.success.clone()
        }
    }
}

#[derive(Clone)]
struct RetryAfterThen {
    attempts: Arc<AtomicUsize>,
    success: ResponseTemplate,
}

impl Respond for RetryAfterThen {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429).insert_header("retry-after", "0")
        } else {
            self.success.clone()
        }
    }
}

fn image(server: &MockServer) -> ImageReference {
    ImageReference {
        registry: server.address().to_string(),
        repository: "acme/app".into(),
        reference: "latest".into(),
    }
}

#[test]
fn parses_bearer_parameters_in_arbitrary_order_with_quoted_commas() {
    let parsed = parse_bearer_challenge(
        r#"Bearer scope="repository:acme/app:pull,push", service="registry.example", realm="https://auth.example/token""#,
    ).unwrap();
    assert_eq!(parsed.realm.as_str(), "https://auth.example/token");
    assert_eq!(parsed.service.as_deref(), Some("registry.example"));
    assert_eq!(
        parsed.scope.as_deref(),
        Some("repository:acme/app:pull,push")
    );
}

#[tokio::test]
async fn resolves_oci_index_and_preserves_digest_reference_and_layer_order() {
    let server = MockServer::start().await;
    let manifest_digest = digest(MANIFEST);
    let index = index("application/vnd.oci.image.index.v1+json", &manifest_digest);
    let index_digest = digest(&index);
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(NoAuthorization)
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.as_str())
                .set_body_raw(index, "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/acme/app/manifests/{manifest_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .mount(&server)
        .await;

    let resolved = RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "arm".into(),
                variant: Some("v7".into()),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(resolved.manifest_digest, manifest_digest);
    assert_eq!(resolved.repository, "acme/app");
    assert_eq!(resolved.reference, "latest");
    assert_eq!(resolved.platform.variant.as_deref(), Some("v7"));
    assert_eq!(
        resolved.config.media_type,
        "application/vnd.oci.image.config.v1+json"
    );
    assert_eq!(resolved.config.size, 10);
    assert_eq!(
        resolved.layers[0].digest,
        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    );
    assert_eq!(
        resolved.layers[1].digest,
        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    );
}

#[tokio::test]
async fn token_query_parameters_are_encoded_and_auth_is_not_sent_to_manifest_endpoint() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(NoAuthorization)
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(
                r#"Bearer service="registry.test",realm="{realm}",scope="repository:acme/app:pull""#
            ),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("service", "registry.test"))
        .and(query_param("scope", "repository:acme/app:pull"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"token-value"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(header("authorization", "Bearer token-value"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest(MANIFEST).as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let session = RegistryClient::new()
        .unwrap()
        .resolve_session(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(session.blob_authorization(), Some("Bearer token-value"));
}

#[tokio::test]
async fn obtains_anonymous_bearer_token_without_basic_auth() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(NoAuthorization)
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", format!(r#"Bearer realm="{realm}""#)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"access_token":"anonymous-token"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(header("authorization", "Bearer anonymous-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest(MANIFEST).as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn maps_registry_status_and_missing_platform_errors() {
    for (status, code) in [(401, "REGISTRY_AUTH_FAILED"), (404, "REGISTRY_NOT_FOUND")] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let error = RegistryClient::new()
            .unwrap()
            .resolve(
                &image(&server),
                Platform {
                    os: "linux".into(),
                    architecture: "amd64".into(),
                    variant: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), code);
    }
    let server = MockServer::start().await;
    let index = index(
        "application/vnd.docker.distribution.manifest.list.v2+json",
        &digest(MANIFEST),
    );
    let index_digest = digest(&index);
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", index_digest.as_str())
                .set_body_raw(
                    index,
                    "application/vnd.docker.distribution.manifest.list.v2+json",
                ),
        )
        .mount(&server)
        .await;
    let error = RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "windows".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::PlatformNotFound));
}

#[tokio::test]
async fn resolves_docker_manifest_list() {
    let server = MockServer::start().await;
    let manifest_digest = digest(MANIFEST);
    let list = index(
        "application/vnd.docker.distribution.manifest.list.v2+json",
        &manifest_digest,
    );
    let list_digest = digest(&list);
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", list_digest.as_str())
                .set_body_raw(
                    list,
                    "application/vnd.docker.distribution.manifest.list.v2+json",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/acme/app/manifests/{manifest_digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.docker.distribution.manifest.v2+json",
                ),
        )
        .mount(&server)
        .await;
    let resolved = RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(resolved.manifest_digest, manifest_digest);
    assert_eq!(
        resolved.config.digest,
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    );
}

#[tokio::test]
async fn retries_manifest_get_on_500() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let manifest_digest = digest(MANIFEST);
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .respond_with(RetryThen {
            attempts: attempts.clone(),
            retry_status: 500,
            success: ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        })
        .mount(&server)
        .await;
    RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retries_429_with_numeric_retry_after() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let manifest_digest = digest(MANIFEST);
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .respond_with(RetryAfterThen {
            attempts: attempts.clone(),
            success: ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        })
        .mount(&server)
        .await;
    RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retries_token_get_on_429() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let realm = format!("{}/token", server.uri());
    let manifest_digest = digest(MANIFEST);
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(NoAuthorization)
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", format!(r#"Bearer realm="{realm}""#)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(RetryThen {
            attempts: attempts.clone(),
            retry_status: 429,
            success: ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"token":"retry-token"})),
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(header("authorization", "Bearer retry-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .mount(&server)
        .await;
    RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retries_head_on_server_error() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("HEAD"))
        .and(path("/probe"))
        .respond_with(RetryThen {
            attempts: attempts.clone(),
            retry_status: 503,
            success: ResponseTemplate::new(200),
        })
        .mount(&server)
        .await;
    let status = RegistryClient::new()
        .unwrap()
        .head(url::Url::parse(&format!("{}/probe", server.uri())).unwrap())
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn rejects_manifest_when_response_bytes_do_not_match_digest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "docker-content-digest",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .mount(&server)
        .await;
    let error = RegistryClient::new()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "DIGEST_MISMATCH");
}

#[test]
fn token_realm_policy_rejects_plaintext_credentials_and_cross_host() {
    let registry = url::Url::parse("https://registry.example/v2/").unwrap();
    assert!(
        validate_token_realm(
            &registry,
            &url::Url::parse("https://registry.example/token").unwrap(),
            true
        )
        .is_ok()
    );
    assert_eq!(
        validate_token_realm(
            &registry,
            &url::Url::parse("http://registry.example/token").unwrap(),
            true
        )
        .unwrap_err()
        .code(),
        "REGISTRY_AUTH_FAILED"
    );
    assert_eq!(
        validate_token_realm(
            &registry,
            &url::Url::parse("https://evil.example/token").unwrap(),
            false
        )
        .unwrap_err()
        .code(),
        "REGISTRY_AUTH_FAILED"
    );
}

#[test]
fn docker_hub_allows_only_its_https_token_realm_for_anonymous_and_basic_auth() {
    for registry in ["https://registry-1.docker.io", "https://docker.io"] {
        let registry = url::Url::parse(registry).unwrap();
        let realm = url::Url::parse("https://auth.docker.io/token").unwrap();
        assert!(validate_token_realm(&registry, &realm, false).is_ok());
        assert!(validate_token_realm(&registry, &realm, true).is_ok());
    }

    let registry = url::Url::parse("https://registry-1.docker.io").unwrap();
    for realm in [
        "http://auth.docker.io/token",
        "https://evil.example/token",
        "https://auth.docker.io:444/token",
        "https://user:pass@auth.docker.io/token",
    ] {
        assert!(
            validate_token_realm(&registry, &url::Url::parse(realm).unwrap(), true).is_err(),
            "unexpectedly accepted {realm}"
        );
    }

    let realm = url::Url::parse("https://auth.docker.io/token").unwrap();
    for registry in [
        "http://registry-1.docker.io",
        "https://registry-1.docker.io:444",
        "https://user:pass@registry-1.docker.io",
    ] {
        assert!(
            validate_token_realm(&url::Url::parse(registry).unwrap(), &realm, true).is_err(),
            "unexpectedly accepted Docker Hub source {registry}"
        );
    }
}

/// Run with `cargo test --features test-utils --test registry`.
#[cfg(feature = "test-utils")]
#[tokio::test]
async fn basic_credentials_are_sent_only_to_token_endpoint() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    let manifest_digest = digest(MANIFEST);

    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(NoAuthorization)
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", format!(r#"Bearer realm="{realm}""#)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(basic_auth("user", "pass"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"manifest-token"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/manifests/latest"))
        .and(header("authorization", "Bearer manifest-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", manifest_digest.as_str())
                .set_body_raw(
                    MANIFEST.as_bytes(),
                    "application/vnd.oci.image.manifest.v1+json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let resolved = RegistryClient::new_for_loopback_basic_test()
        .unwrap()
        .resolve(
            &image(&server),
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            },
            Some(("user", "pass")),
        )
        .await
        .unwrap();
    assert_eq!(resolved.manifest_digest, manifest_digest);
}
